//! Handles lowering of build-system specific workspace information (`cargo
//! metadata` or `rust-project.json`) into representation stored in the salsa
//! database -- `CrateGraph`.

use std::{
    fmt, fs,
    path::{Component, Path},
    process::Command,
};

use anyhow::{Context, Result};
use base_db::{CrateDisplayName, CrateGraph, CrateId, CrateName, Edition, Env, FileId, ProcMacro};
use cfg::CfgOptions;
use paths::{AbsPath, AbsPathBuf};
use proc_macro_api::ProcMacroClient;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    cargo_workspace, cfg_flag::CfgFlag, sysroot::SysrootCrate, utf8_stdout, CargoConfig,
    CargoWorkspace, ProjectJson, ProjectManifest, Sysroot, TargetKind,
};

/// `PackageRoot` describes a package root folder.
/// Which may be an external dependency, or a member of
/// the current workspace.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PackageRoot {
    /// Is a member of the current workspace
    pub is_member: bool,
    pub include: Vec<AbsPathBuf>,
    pub exclude: Vec<AbsPathBuf>,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ProjectWorkspace {
    /// Project workspace was discovered by running `cargo metadata` and `rustc --print sysroot`.
    Cargo { cargo: CargoWorkspace, sysroot: Sysroot, rustc: Option<CargoWorkspace> },
    /// Project workspace was manually specified using a `rust-project.json` file.
    Json { project: ProjectJson, sysroot: Option<Sysroot> },
}

impl fmt::Debug for ProjectWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectWorkspace::Cargo { cargo, sysroot, rustc } => f
                .debug_struct("Cargo")
                .field("n_packages", &cargo.packages().len())
                .field("n_sysroot_crates", &sysroot.crates().len())
                .field(
                    "n_rustc_compiler_crates",
                    &rustc.as_ref().map_or(0, |rc| rc.packages().len()),
                )
                .finish(),
            ProjectWorkspace::Json { project, sysroot } => {
                let mut debug_struct = f.debug_struct("Json");
                debug_struct.field("n_crates", &project.n_crates());
                if let Some(sysroot) = sysroot {
                    debug_struct.field("n_sysroot_crates", &sysroot.crates().len());
                }
                debug_struct.finish()
            }
        }
    }
}

impl ProjectWorkspace {
    pub fn load(manifest: ProjectManifest, config: &CargoConfig) -> Result<ProjectWorkspace> {
        let res = match manifest {
            ProjectManifest::ProjectJson(project_json) => {
                let file = fs::read_to_string(&project_json).with_context(|| {
                    format!("Failed to read json file {}", project_json.display())
                })?;
                let data = serde_json::from_str(&file).with_context(|| {
                    format!("Failed to deserialize json file {}", project_json.display())
                })?;
                let project_location = project_json.parent().unwrap().to_path_buf();
                let project_json = ProjectJson::new(&project_location, data);
                ProjectWorkspace::load_inline(project_json)?
            }
            ProjectManifest::CargoToml(cargo_toml) => {
                let cargo_version = utf8_stdout({
                    let mut cmd = Command::new(toolchain::cargo());
                    cmd.arg("--version");
                    cmd
                })?;

                let cargo = CargoWorkspace::from_cargo_metadata(&cargo_toml, config).with_context(
                    || {
                        format!(
                            "Failed to read Cargo metadata from Cargo.toml file {}, {}",
                            cargo_toml.display(),
                            cargo_version
                        )
                    },
                )?;
                let sysroot = if config.no_sysroot {
                    Sysroot::default()
                } else {
                    Sysroot::discover(&cargo_toml).with_context(|| {
                        format!(
                            "Failed to find sysroot for Cargo.toml file {}. Is rust-src installed?",
                            cargo_toml.display()
                        )
                    })?
                };

                let rustc = if let Some(rustc_dir) = &config.rustc_source {
                    Some(CargoWorkspace::from_cargo_metadata(&rustc_dir, config).with_context(
                        || format!("Failed to read Cargo metadata for Rust sources"),
                    )?)
                } else {
                    None
                };

                ProjectWorkspace::Cargo { cargo, sysroot, rustc }
            }
        };

        Ok(res)
    }

    pub fn load_inline(project_json: ProjectJson) -> Result<ProjectWorkspace> {
        let sysroot = match &project_json.sysroot_src {
            Some(path) => Some(Sysroot::load(path)?),
            None => None,
        };

        Ok(ProjectWorkspace::Json { project: project_json, sysroot })
    }

    /// Returns the roots for the current `ProjectWorkspace`
    /// The return type contains the path and whether or not
    /// the root is a member of the current workspace
    pub fn to_roots(&self) -> Vec<PackageRoot> {
        match self {
            ProjectWorkspace::Json { project, sysroot } => project
                .crates()
                .map(|(_, krate)| PackageRoot {
                    is_member: krate.is_workspace_member,
                    include: krate.include.clone(),
                    exclude: krate.exclude.clone(),
                })
                .collect::<FxHashSet<_>>()
                .into_iter()
                .chain(sysroot.as_ref().into_iter().flat_map(|sysroot| {
                    sysroot.crates().map(move |krate| PackageRoot {
                        is_member: false,
                        include: vec![sysroot[krate].root_dir().to_path_buf()],
                        exclude: Vec::new(),
                    })
                }))
                .collect::<Vec<_>>(),
            ProjectWorkspace::Cargo { cargo, sysroot, rustc } => cargo
                .packages()
                .map(|pkg| {
                    let is_member = cargo[pkg].is_member;
                    let pkg_root = cargo[pkg].root().to_path_buf();

                    let mut include = vec![pkg_root.clone()];
                    include.extend(cargo[pkg].out_dir.clone());

                    let mut exclude = vec![pkg_root.join(".git")];
                    if is_member {
                        exclude.push(pkg_root.join("target"));
                    } else {
                        exclude.push(pkg_root.join("tests"));
                        exclude.push(pkg_root.join("examples"));
                        exclude.push(pkg_root.join("benches"));
                    }
                    PackageRoot { is_member, include, exclude }
                })
                .chain(sysroot.crates().map(|krate| PackageRoot {
                    is_member: false,
                    include: vec![sysroot[krate].root_dir().to_path_buf()],
                    exclude: Vec::new(),
                }))
                .chain(rustc.into_iter().flat_map(|rustc| {
                    rustc.packages().map(move |krate| PackageRoot {
                        is_member: false,
                        include: vec![rustc[krate].root().to_path_buf()],
                        exclude: Vec::new(),
                    })
                }))
                .collect(),
        }
    }

    pub fn n_packages(&self) -> usize {
        match self {
            ProjectWorkspace::Json { project, .. } => project.n_crates(),
            ProjectWorkspace::Cargo { cargo, sysroot, rustc } => {
                let rustc_package_len = rustc.as_ref().map_or(0, |rc| rc.packages().len());
                cargo.packages().len() + sysroot.crates().len() + rustc_package_len
            }
        }
    }

    pub fn to_crate_graph(
        &self,
        target: Option<&str>,
        proc_macro_client: Option<&ProcMacroClient>,
        load: &mut dyn FnMut(&AbsPath) -> Option<FileId>,
    ) -> CrateGraph {
        let proc_macro_loader = |path: &Path| match proc_macro_client {
            Some(client) => client.by_dylib_path(path),
            None => Vec::new(),
        };

        let mut crate_graph = match self {
            ProjectWorkspace::Json { project, sysroot } => {
                project_json_to_crate_graph(target, &proc_macro_loader, load, project, sysroot)
            }
            ProjectWorkspace::Cargo { cargo, sysroot, rustc } => {
                cargo_to_crate_graph(target, &proc_macro_loader, load, cargo, sysroot, rustc)
            }
        };
        if crate_graph.patch_cfg_if() {
            log::debug!("Patched std to depend on cfg-if")
        } else {
            log::debug!("Did not patch std to depend on cfg-if")
        }
        crate_graph
    }
}

fn project_json_to_crate_graph(
    target: Option<&str>,
    proc_macro_loader: &dyn Fn(&Path) -> Vec<ProcMacro>,
    load: &mut dyn FnMut(&AbsPath) -> Option<FileId>,
    project: &ProjectJson,
    sysroot: &Option<Sysroot>,
) -> CrateGraph {
    let mut crate_graph = CrateGraph::default();
    let sysroot_deps = sysroot
        .as_ref()
        .map(|sysroot| sysroot_to_crate_graph(&mut crate_graph, sysroot, target, load));

    let mut cfg_cache: FxHashMap<Option<&str>, Vec<CfgFlag>> = FxHashMap::default();
    let crates: FxHashMap<CrateId, CrateId> = project
        .crates()
        .filter_map(|(crate_id, krate)| {
            let file_path = &krate.root_module;
            let file_id = load(&file_path)?;
            Some((crate_id, krate, file_id))
        })
        .map(|(crate_id, krate, file_id)| {
            let env = krate.env.clone().into_iter().collect();
            let proc_macro = krate.proc_macro_dylib_path.clone().map(|it| proc_macro_loader(&it));

            let target = krate.target.as_deref().or(target);
            let target_cfgs =
                cfg_cache.entry(target).or_insert_with(|| get_rustc_cfg_options(target));

            let mut cfg_options = CfgOptions::default();
            cfg_options.extend(target_cfgs.iter().chain(krate.cfg.iter()).cloned());
            (
                crate_id,
                crate_graph.add_crate_root(
                    file_id,
                    krate.edition,
                    krate.display_name.clone(),
                    cfg_options,
                    env,
                    proc_macro.unwrap_or_default(),
                ),
            )
        })
        .collect();

    for (from, krate) in project.crates() {
        if let Some(&from) = crates.get(&from) {
            if let Some((public_deps, _proc_macro)) = &sysroot_deps {
                for (name, to) in public_deps.iter() {
                    add_dep(&mut crate_graph, from, name.clone(), *to)
                }
            }

            for dep in &krate.deps {
                if let Some(&to) = crates.get(&dep.crate_id) {
                    add_dep(&mut crate_graph, from, dep.name.clone(), to)
                }
            }
        }
    }
    crate_graph
}

fn cargo_to_crate_graph(
    target: Option<&str>,
    proc_macro_loader: &dyn Fn(&Path) -> Vec<ProcMacro>,
    load: &mut dyn FnMut(&AbsPath) -> Option<FileId>,
    cargo: &CargoWorkspace,
    sysroot: &Sysroot,
    rustc: &Option<CargoWorkspace>,
) -> CrateGraph {
    let mut crate_graph = CrateGraph::default();
    let (public_deps, libproc_macro) =
        sysroot_to_crate_graph(&mut crate_graph, sysroot, target, load);

    let mut cfg_options = CfgOptions::default();
    cfg_options.extend(get_rustc_cfg_options(target));

    let mut pkg_to_lib_crate = FxHashMap::default();

    // Add test cfg for non-sysroot crates
    cfg_options.insert_atom("test".into());
    cfg_options.insert_atom("debug_assertions".into());

    let mut pkg_crates = FxHashMap::default();

    // Next, create crates for each package, target pair
    for pkg in cargo.packages() {
        let mut lib_tgt = None;
        for &tgt in cargo[pkg].targets.iter() {
            if let Some(file_id) = load(&cargo[tgt].root) {
                let crate_id = add_target_crate_root(
                    &mut crate_graph,
                    &cargo[pkg],
                    &cfg_options,
                    proc_macro_loader,
                    file_id,
                );
                if cargo[tgt].kind == TargetKind::Lib {
                    lib_tgt = Some((crate_id, cargo[tgt].name.clone()));
                    pkg_to_lib_crate.insert(pkg, crate_id);
                }
                if cargo[tgt].is_proc_macro {
                    if let Some(proc_macro) = libproc_macro {
                        add_dep(
                            &mut crate_graph,
                            crate_id,
                            CrateName::new("proc_macro").unwrap(),
                            proc_macro,
                        );
                    }
                }

                pkg_crates.entry(pkg).or_insert_with(Vec::new).push(crate_id);
            }
        }

        // Set deps to the core, std and to the lib target of the current package
        for &from in pkg_crates.get(&pkg).into_iter().flatten() {
            if let Some((to, name)) = lib_tgt.clone() {
                if to != from {
                    // For root projects with dashes in their name,
                    // cargo metadata does not do any normalization,
                    // so we do it ourselves currently
                    let name = CrateName::normalize_dashes(&name);
                    add_dep(&mut crate_graph, from, name, to);
                }
            }
            for (name, krate) in public_deps.iter() {
                add_dep(&mut crate_graph, from, name.clone(), *krate);
            }
        }
    }

    // Now add a dep edge from all targets of upstream to the lib
    // target of downstream.
    for pkg in cargo.packages() {
        for dep in cargo[pkg].dependencies.iter() {
            let name = CrateName::new(&dep.name).unwrap();
            if let Some(&to) = pkg_to_lib_crate.get(&dep.pkg) {
                for &from in pkg_crates.get(&pkg).into_iter().flatten() {
                    add_dep(&mut crate_graph, from, name.clone(), to)
                }
            }
        }
    }

    let mut rustc_pkg_crates = FxHashMap::default();

    // If the user provided a path to rustc sources, we add all the rustc_private crates
    // and create dependencies on them for the crates in the current workspace
    if let Some(rustc_workspace) = rustc {
        for pkg in rustc_workspace.packages() {
            for &tgt in rustc_workspace[pkg].targets.iter() {
                if rustc_workspace[tgt].kind != TargetKind::Lib {
                    continue;
                }
                // Exclude alloc / core / std
                if rustc_workspace[tgt]
                    .root
                    .components()
                    .any(|c| c == Component::Normal("library".as_ref()))
                {
                    continue;
                }

                if let Some(file_id) = load(&rustc_workspace[tgt].root) {
                    let crate_id = add_target_crate_root(
                        &mut crate_graph,
                        &rustc_workspace[pkg],
                        &cfg_options,
                        proc_macro_loader,
                        file_id,
                    );
                    pkg_to_lib_crate.insert(pkg, crate_id);
                    // Add dependencies on the core / std / alloc for rustc
                    for (name, krate) in public_deps.iter() {
                        add_dep(&mut crate_graph, crate_id, name.clone(), *krate);
                    }
                    rustc_pkg_crates.entry(pkg).or_insert_with(Vec::new).push(crate_id);
                }
            }
        }
        // Now add a dep edge from all targets of upstream to the lib
        // target of downstream.
        for pkg in rustc_workspace.packages() {
            for dep in rustc_workspace[pkg].dependencies.iter() {
                let name = CrateName::new(&dep.name).unwrap();
                if let Some(&to) = pkg_to_lib_crate.get(&dep.pkg) {
                    for &from in rustc_pkg_crates.get(&pkg).into_iter().flatten() {
                        add_dep(&mut crate_graph, from, name.clone(), to);
                    }
                }
            }
        }

        // Add dependencies for all the crates of the current workspace to rustc_private libraries
        for dep in rustc_workspace.packages() {
            let name = CrateName::normalize_dashes(&rustc_workspace[dep].name);

            if let Some(&to) = pkg_to_lib_crate.get(&dep) {
                for pkg in cargo.packages() {
                    if !cargo[pkg].is_member {
                        continue;
                    }
                    for &from in pkg_crates.get(&pkg).into_iter().flatten() {
                        add_dep(&mut crate_graph, from, name.clone(), to);
                    }
                }
            }
        }
    }
    crate_graph
}

fn add_target_crate_root(
    crate_graph: &mut CrateGraph,
    pkg: &cargo_workspace::PackageData,
    cfg_options: &CfgOptions,
    proc_macro_loader: &dyn Fn(&Path) -> Vec<ProcMacro>,
    file_id: FileId,
) -> CrateId {
    let edition = pkg.edition;
    let cfg_options = {
        let mut opts = cfg_options.clone();
        for feature in pkg.features.iter() {
            opts.insert_key_value("feature".into(), feature.into());
        }
        opts.extend(pkg.cfgs.iter().cloned());
        opts
    };

    let mut env = Env::default();
    for (k, v) in &pkg.envs {
        env.set(k, v.clone());
    }
    if let Some(out_dir) = &pkg.out_dir {
        // NOTE: cargo and rustc seem to hide non-UTF-8 strings from env! and option_env!()
        if let Some(out_dir) = out_dir.to_str().map(|s| s.to_owned()) {
            env.set("OUT_DIR", out_dir);
        }
    }

    let proc_macro =
        pkg.proc_macro_dylib_path.as_ref().map(|it| proc_macro_loader(&it)).unwrap_or_default();

    let display_name = CrateDisplayName::from_canonical_name(pkg.name.clone());
    let crate_id = crate_graph.add_crate_root(
        file_id,
        edition,
        Some(display_name),
        cfg_options,
        env,
        proc_macro,
    );

    crate_id
}

fn sysroot_to_crate_graph(
    crate_graph: &mut CrateGraph,
    sysroot: &Sysroot,
    target: Option<&str>,
    load: &mut dyn FnMut(&AbsPath) -> Option<FileId>,
) -> (Vec<(CrateName, CrateId)>, Option<CrateId>) {
    let mut cfg_options = CfgOptions::default();
    cfg_options.extend(get_rustc_cfg_options(target));
    let sysroot_crates: FxHashMap<SysrootCrate, CrateId> = sysroot
        .crates()
        .filter_map(|krate| {
            let file_id = load(&sysroot[krate].root)?;

            let env = Env::default();
            let proc_macro = vec![];
            let display_name = CrateDisplayName::from_canonical_name(sysroot[krate].name.clone());
            let crate_id = crate_graph.add_crate_root(
                file_id,
                Edition::Edition2018,
                Some(display_name),
                cfg_options.clone(),
                env,
                proc_macro,
            );
            Some((krate, crate_id))
        })
        .collect();

    for from in sysroot.crates() {
        for &to in sysroot[from].deps.iter() {
            let name = CrateName::new(&sysroot[to].name).unwrap();
            if let (Some(&from), Some(&to)) = (sysroot_crates.get(&from), sysroot_crates.get(&to)) {
                add_dep(crate_graph, from, name, to);
            }
        }
    }

    let public_deps = sysroot
        .public_deps()
        .map(|(name, idx)| (CrateName::new(name).unwrap(), sysroot_crates[&idx]))
        .collect::<Vec<_>>();

    let libproc_macro = sysroot.proc_macro().and_then(|it| sysroot_crates.get(&it).copied());
    (public_deps, libproc_macro)
}

fn get_rustc_cfg_options(target: Option<&str>) -> Vec<CfgFlag> {
    let mut res = Vec::new();

    // Some nightly-only cfgs, which are required for stdlib
    res.push(CfgFlag::Atom("target_thread_local".into()));
    for &ty in ["8", "16", "32", "64", "cas", "ptr"].iter() {
        for &key in ["target_has_atomic", "target_has_atomic_load_store"].iter() {
            res.push(CfgFlag::KeyValue { key: key.to_string(), value: ty.into() });
        }
    }

    let rustc_cfgs = {
        let mut cmd = Command::new(toolchain::rustc());
        cmd.args(&["--print", "cfg", "-O"]);
        if let Some(target) = target {
            cmd.args(&["--target", target]);
        }
        utf8_stdout(cmd)
    };

    match rustc_cfgs {
        Ok(rustc_cfgs) => res.extend(rustc_cfgs.lines().map(|it| it.parse().unwrap())),
        Err(e) => log::error!("failed to get rustc cfgs: {:#}", e),
    }

    res
}

fn add_dep(graph: &mut CrateGraph, from: CrateId, name: CrateName, to: CrateId) {
    if let Err(err) = graph.add_dep(from, name, to) {
        log::error!("{}", err)
    }
}
