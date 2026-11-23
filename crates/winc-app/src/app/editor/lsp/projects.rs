//! 在后台确定 Rust 文件的 Cargo 工作区、项目清单或独立入口。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use winc_editor::shell::Shell;

use super::client::{Project, Server};

type Discovery = (PathBuf, Project);

#[derive(Default)]
pub(super) struct RustProjects {
    projects: HashMap<PathBuf, Option<Project>>,
    worker: Option<(Sender<PathBuf>, Receiver<Discovery>)>,
}

impl RustProjects {
    pub fn requested(
        &mut self,
        path: &Path,
        folders: &[PathBuf],
        shell: &Shell,
        wake: impl Fn() + Send + 'static,
    ) -> Option<Project> {
        if !self.projects.contains_key(path) {
            let (send, _) = self
                .worker
                .get_or_insert_with(|| start(shell.clone(), wake));
            if send.send(path.to_owned()).is_ok() {
                self.projects.insert(path.to_owned(), None);
            } else {
                self.worker = None;
            }
        }
        let project = self.projects.get(path)?.as_ref()?;
        Some(if project.is_standalone() {
            Project::standalone(standalone_root(path, folders).to_owned())
        } else {
            project.clone()
        })
    }

    pub fn poll(&mut self) {
        if let Some((_, results)) = &self.worker {
            for (path, project) in results.try_iter() {
                self.projects.insert(path, Some(project));
            }
        }
    }
}

fn start(shell: Shell, wake: impl Fn() + Send + 'static) -> (Sender<PathBuf>, Receiver<Discovery>) {
    let (request, paths) = mpsc::channel::<PathBuf>();
    let (reply, results) = mpsc::channel();
    winc_editor::background::spawn_blocking(move || {
        let mut manifests = HashMap::new();
        while let Ok(path) = paths.recv() {
            let project = rust_project(&path, &shell, &mut manifests);
            if reply.send((path, project)).is_err() {
                break;
            }
            wake();
        }
    });
    (request, results)
}

pub(super) fn rust_project(
    path: &Path,
    shell: &Shell,
    manifests: &mut HashMap<PathBuf, PathBuf>,
) -> Project {
    let parent = path.parent().unwrap_or(path);
    for dir in parent.ancestors() {
        let json = dir.join("rust-project.json");
        if winc_backend::fs::is_file(&json) {
            return Project::rust(dir.to_owned(), json);
        }
        let cargo = dir.join("Cargo.toml");
        if winc_backend::fs::is_file(&cargo) {
            let manifest = manifests
                .entry(cargo.clone())
                .or_insert_with(|| shell.cargo_workspace_manifest(&cargo).unwrap_or(cargo));
            return Project::rust(
                manifest.parent().unwrap_or(dir).to_owned(),
                manifest.clone(),
            );
        }
    }
    Project::standalone(parent.to_owned())
}

pub(super) fn standalone_root<'a>(path: &'a Path, folders: &'a [PathBuf]) -> &'a Path {
    folders
        .iter()
        .map(PathBuf::as_path)
        .filter(|folder| path.starts_with(folder))
        .max_by_key(|folder| folder.components().count())
        .unwrap_or_else(|| path.parent().unwrap_or(path))
}

pub(super) fn teal_project(folders: &[PathBuf], path: &Path) -> Project {
    // Teal 逐文档找所属包。同窗口打开的路径依赖需要共享未保存的内容。
    let root = folders
        .first()
        .map_or_else(|| path.parent().unwrap_or(path).to_owned(), Clone::clone);
    Project {
        server: Server::Teal,
        root,
        rust_entry: None,
        extra_folders: folders.get(1..).unwrap_or_default().to_vec(),
    }
}
