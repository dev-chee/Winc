//! 动态文件监听协议，范围由 Teal 的项目分析结果决定。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;

use super::protocol::{file_path, file_uri};

const METHOD: &str = "workspace/didChangeWatchedFiles";

#[derive(Clone)]
struct Rule {
    base: PathBuf,
    pattern: GlobMatcher,
    mode: RecursiveMode,
    kinds: u64,
}

pub(super) struct Watches {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
    registered: HashMap<String, Vec<Rule>>,
    applied: Vec<Rule>,
    watched: HashMap<PathBuf, RecursiveMode>,
    rearm: HashSet<PathBuf>,
    needs_refresh: bool,
    healthy: bool,
}

impl Watches {
    pub fn new(roots: Vec<PathBuf>, send: impl Fn(Event) + Send + 'static) -> notify::Result<Self> {
        let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            if let Ok(event) = event {
                send(event);
            } else {
                send(Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan));
            }
        })?;
        Ok(Self {
            watcher,
            roots,
            registered: HashMap::new(),
            applied: Vec::new(),
            watched: HashMap::new(),
            rearm: HashSet::new(),
            needs_refresh: false,
            healthy: true,
        })
    }

    pub fn covers(&self, path: &Path, kind: u8) -> bool {
        self.healthy && self.rearm.is_empty() && self.matches(path, kind)
    }

    pub fn register(&mut self, params: &Value) -> Result<(), String> {
        let registrations = params["registrations"]
            .as_array()
            .ok_or("missing registrations")?;
        let mut parsed = Vec::new();
        for item in registrations {
            if item["method"] != METHOD {
                return Err("unsupported registration method".into());
            }
            let id = item["id"].as_str().ok_or("missing registration id")?;
            let watches = item["registerOptions"]["watchers"]
                .as_array()
                .ok_or("missing watchers")?;
            let mut rules = Vec::new();
            for watch in watches {
                let glob = &watch["globPattern"];
                let kinds = watch["kind"].as_u64().unwrap_or(7);
                if let Some(pattern) = glob.as_str() {
                    for base in &self.roots {
                        rules.push(rule(base.clone(), pattern, kinds)?);
                    }
                } else {
                    let uri = glob["baseUri"]
                        .as_str()
                        .or_else(|| glob["baseUri"]["uri"].as_str())
                        .ok_or("missing watch base URI")?;
                    let base = file_path(uri)
                        .filter(|path| !winc_backend::is_remote(path))
                        .ok_or("unsupported watch base URI")?;
                    let pattern = glob["pattern"].as_str().ok_or("missing watch glob")?;
                    rules.push(rule(winc_editor::canonical_path(&base), pattern, kinds)?);
                }
            }
            parsed.push((id.to_owned(), rules));
        }
        for (id, rules) in parsed {
            self.registered.insert(id, rules);
        }
        self.needs_refresh = true;
        Ok(())
    }

    pub fn unregister(&mut self, params: &Value) -> Result<(), String> {
        // LSP 3.x 的字段保留了拼写；也接受修正过的字段名。
        let registrations = params["unregisterations"]
            .as_array()
            .or_else(|| params["unregistrations"].as_array())
            .ok_or("missing unregistrations")?;
        for item in registrations {
            if item["method"] == METHOD
                && let Some(id) = item["id"].as_str()
            {
                self.needs_refresh |= self.registered.remove(id).is_some();
            }
        }
        Ok(())
    }

    /// 在通知合并窗口结束后协调规则与句柄，返回是否需要补查覆盖范围。
    pub fn refresh(&mut self) -> bool {
        if !std::mem::take(&mut self.needs_refresh) {
            return false;
        }
        let previous: HashSet<_> = self.applied.iter().map(Rule::key).collect();
        let mut catch_up = self
            .registered
            .values()
            .flatten()
            .any(|rule| !previous.contains(&rule.key()))
            || !self.rearm.is_empty();
        let mut desired = HashMap::new();
        for rule in self.registered.values().flatten() {
            // 父目录只监听直接子项。被替换的目录本身可能没有机会报告事件。
            for base in rule
                .base
                .ancestors()
                .filter(|p| winc_backend::fs::is_dir(p))
            {
                let mode = if base == rule.base {
                    rule.mode
                } else {
                    RecursiveMode::NonRecursive
                };
                let current = desired.entry(base.to_owned()).or_insert(mode);
                if mode == RecursiveMode::Recursive {
                    *current = mode;
                }
            }
        }
        self.watched.retain(|path, current| {
            if desired.get(path) == Some(current) && !self.rearm.contains(path) {
                true
            } else {
                let _ = self.watcher.unwatch(path);
                false
            }
        });
        self.rearm.clear();
        self.healthy = true;
        for (path, mode) in desired {
            if !self.watched.contains_key(&path) {
                if self.watcher.watch(&path, mode).is_ok() {
                    self.watched.insert(path, mode);
                    catch_up = true;
                } else {
                    self.healthy = false;
                }
            }
        }
        self.applied = self.registered.values().flatten().cloned().collect();
        catch_up
    }

    pub fn changes(&mut self, event: Event, changes: &mut HashMap<String, u8>) -> bool {
        let mut catch_up = event.need_rescan();
        if catch_up {
            self.rearm.extend(self.watched.keys().cloned());
        }
        for (i, path) in event.paths.into_iter().enumerate() {
            if matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(ModifyKind::Name(_))
            ) && self.applied.iter().any(|rule| rule.base.starts_with(&path))
            {
                // 路径已经重新出现时，旧句柄仍可能指向刚搬走的目录。
                self.rearm.extend(
                    self.watched
                        .keys()
                        .filter(|base| base.starts_with(&path))
                        .cloned(),
                );
                catch_up = true;
            }
            let kind = match event.kind {
                EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => 1,
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => 3,
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if i == 0 => 3,
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => 1,
                EventKind::Modify(ModifyKind::Name(_)) if !winc_backend::fs::exists(&path) => 3,
                EventKind::Modify(ModifyKind::Metadata(_)) | EventKind::Access(_) => continue,
                _ => 2,
            };
            if self.matches(&path, kind)
                && let Some(uri) = file_uri(&path)
            {
                changes.insert(uri, kind);
            }
        }
        self.needs_refresh |= catch_up;
        catch_up
    }

    fn matches(&self, path: &Path, kind: u8) -> bool {
        self.applied.iter().any(|rule| {
            rule.kinds & (1 << (kind - 1)) != 0
                && path
                    .strip_prefix(&rule.base)
                    .is_ok_and(|relative| rule.pattern.is_match(relative))
        })
    }
}

impl Rule {
    fn key(&self) -> (&Path, &str, u64) {
        (&self.base, self.pattern.glob().glob(), self.kinds)
    }
}

fn rule(base: PathBuf, pattern: &str, kinds: u64) -> Result<Rule, String> {
    let mode = if pattern.contains('/') || pattern.contains("**") {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    let pattern = GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(cfg!(windows))
        .build()
        .map_err(|e| e.to_string())?
        .compile_matcher();
    Ok(Rule {
        base,
        pattern,
        mode,
        kinds,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn coalesced_registration_preserves_coverage_and_directory_events_rearm_it() {
        let base = winc_editor::canonical_path(&std::env::temp_dir())
            .join(format!("winc-watch-rearm-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let uri = file_uri(&base).unwrap();
        let mut watches = Watches::new(Vec::new(), |_| {}).unwrap();
        let registration = |id| {
            json!({"registrations":[{
                "id":id, "method":METHOD, "registerOptions":{"watchers":[
                    {"globPattern":{"baseUri":uri,"pattern":"lib.tl"}}
                ]}
            }]})
        };
        watches.register(&registration("old")).unwrap();
        assert!(watches.refresh());
        assert!(watches.watched.contains_key(&base));
        assert!(watches.watched.contains_key(base.parent().unwrap()));
        let current = watches.watched.clone();
        watches
            .unregister(&json!({"unregisterations":[{"id":"old","method":METHOD}]}))
            .unwrap();
        watches.register(&registration("new")).unwrap();
        assert!(!watches.refresh());
        assert_eq!(watches.watched, current);

        let rename = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path(base.clone());
        assert!(watches.changes(rename, &mut HashMap::new()));
        assert!(watches.rearm.contains(&base));
        assert!(watches.refresh());
        assert!(watches.rearm.is_empty());
        assert!(!watches.refresh());
    }
}
