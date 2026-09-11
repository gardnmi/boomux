//! Desktop presentation state only. Never starts Shells or stores terminal data.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read, Write as _},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::PathBuf,
};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_PANES: usize = 4096;
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Arrangement {
    pub tree: Option<Tree>,
    pub floating: Vec<Floating>,
    pub panes: BTreeMap<u64, Pane>,
    pub focused: Option<u64>,
    pub expanded: Option<u64>,
    pub canvas: [f32; 2],
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    // Remote keys contain both owner and resource. Local keys are scoped by
    // Document.owner, verified against the connected daemon before restoring.
    pub shell: Option<String>,
    pub workspace: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Tree {
    Pane(u64),
    Split {
        horizontal: bool,
        ratio: f32,
        first: Box<Tree>,
        second: Box<Tree>,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Floating {
    pub pane: u64,
    pub rect: [f32; 4],
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub version: u32,
    pub revision: String,
    pub owner: String,
    pub active: String,
    pub arrangements: BTreeMap<String, Arrangement>,
    pub minimized: Vec<String>,
    pub workspace_order: Vec<String>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            version: 1,
            revision: String::new(),
            owner: String::new(),
            active: String::new(),
            arrangements: BTreeMap::new(),
            minimized: Vec::new(),
            workspace_order: Vec::new(),
        }
    }
}
impl Arrangement {
    /// Unbound UI placeholders have no terminal identity to restore. Retain
    /// references to unavailable Shells, which can still be reconnected later.
    pub fn discard_unbound_panes(&mut self) {
        self.panes.retain(|_, pane| pane.shell.is_some());
        fn prune(tree: Tree, panes: &BTreeMap<u64, Pane>) -> Option<Tree> {
            match tree {
                Tree::Pane(id) => panes.contains_key(&id).then_some(Tree::Pane(id)),
                Tree::Split {
                    horizontal,
                    ratio,
                    first,
                    second,
                } => match (prune(*first, panes), prune(*second, panes)) {
                    (Some(first), Some(second)) => Some(Tree::Split {
                        horizontal,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (first, second) => first.or(second),
                },
            }
        }
        self.tree = self.tree.take().and_then(|tree| prune(tree, &self.panes));
        self.floating
            .retain(|pane| self.panes.contains_key(&pane.pane));
        self.focused = self
            .focused
            .filter(|id| self.panes.contains_key(id))
            .or_else(|| self.panes.keys().next().copied());
        self.expanded = self.expanded.filter(|id| self.panes.contains_key(id));
    }
}

impl Tree {
    fn validate(&self, depth: usize, ids: &mut HashSet<u64>) -> Result<(), String> {
        if depth > 64 || ids.len() >= MAX_PANES {
            return Err("layout tree exceeds bounds".into());
        }
        match self {
            Self::Pane(id) => {
                if !ids.insert(*id) {
                    return Err("duplicate pane".into());
                }
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || !(0.01..=0.99).contains(ratio) {
                    return Err("invalid split ratio".into());
                }
                first.validate(depth + 1, ids)?;
                second.validate(depth + 1, ids)?;
            }
        }
        Ok(())
    }
}
impl Document {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported layout state version; saved file retained".into());
        }
        if self.arrangements.len() > 256
            || self.minimized.len() > MAX_PANES
            || self.workspace_order.len() > 4096
        {
            return Err("layout state exceeds bounds".into());
        }
        let mut total = 0;
        for arrangement in self.arrangements.values() {
            let mut ids = HashSet::new();
            if let Some(tree) = &arrangement.tree {
                tree.validate(0, &mut ids)?;
            }
            for floating in &arrangement.floating {
                if !ids.insert(floating.pane)
                    || !floating.rect.iter().all(|v| v.is_finite())
                    || floating.rect[2] <= 0.0
                    || floating.rect[3] <= 0.0
                {
                    return Err("invalid floating pane".into());
                }
            }
            total += ids.len();
            if total > MAX_PANES
                || ids.len() != arrangement.panes.len()
                || !ids.iter().all(|id| arrangement.panes.contains_key(id))
            {
                return Err("invalid pane references".into());
            }
            for id in [arrangement.focused, arrangement.expanded]
                .into_iter()
                .flatten()
            {
                if !ids.contains(&id) {
                    return Err("invalid focused or expanded pane".into());
                }
            }
            if !arrangement.canvas.iter().all(|v| v.is_finite() && *v > 0.0) {
                return Err("invalid canvas dimensions".into());
            }
            let mut shells = HashSet::new();
            for pane in arrangement.panes.values() {
                if let Some(shell) = &pane.shell
                    && (shell.len() > 1024 || !shells.insert(shell))
                {
                    return Err("duplicate or oversized Shell identity".into());
                }
            }
        }
        Ok(())
    }
}
pub fn path() -> Option<PathBuf> {
    let root = std::env::var_os("BOOMUX_STATE_HOME")
        .or_else(|| std::env::var_os("XDG_STATE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    root.is_absolute()
        .then(|| root.join("boomux-desktop/layout-state.json"))
}
fn read(path: &PathBuf) -> Result<Document, String> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Document::default()),
        Err(e) => return Err(e.to_string()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("layout state exceeds 2 MiB".into());
    }
    let document: Document = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    document.validate()?;
    Ok(document)
}
struct Store {
    path: PathBuf,
    revision: String,
}
impl Store {
    fn save(&mut self, mut document: Document) -> Result<(), String> {
        document.validate()?;
        let parent = self.path.parent().ok_or("invalid layout path")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("another Desktop is saving this layout".into());
        }
        if read(&self.path)?.revision != self.revision {
            return Err("another Desktop changed this layout; reopen Desktop before saving".into());
        }
        document.revision = uuid::Uuid::new_v4().to_string();
        let bytes = serde_json::to_vec(&document).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err("layout state exceeds 2 MiB".into());
        }
        let temporary = parent.join(format!(".layout-{}", document.revision));
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            fs::File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result.map_err(|e| e.to_string())?;
        self.revision = document.revision;
        Ok(())
    }
}
pub struct Write {
    pub document: Document,
    pub completion: async_channel::Sender<Result<(), String>>,
}
pub struct Session {
    pub document: Document,
    pub writer: Option<async_channel::Sender<Write>>,
    pub error: Option<String>,
}
impl Session {
    pub fn load() -> Self {
        Self::at(path())
    }
    fn at(path: Option<PathBuf>) -> Self {
        let result = path
            .ok_or("cannot resolve Desktop state directory".into())
            .and_then(|path| read(&path).map(|doc| (path, doc)));
        match result {
            Err(error) => Self {
                document: Document::default(),
                writer: None,
                error: Some(error),
            },
            Ok((path, document)) => {
                let mut store = Store {
                    path,
                    revision: document.revision.clone(),
                };
                let (send, receive) = async_channel::bounded::<Write>(1);
                std::thread::spawn(move || {
                    while let Ok(request) = receive.recv_blocking() {
                        let result = store.save(request.document);
                        let _ = request.completion.send_blocking(result);
                    }
                });
                Self {
                    document,
                    writer: Some(send),
                    error: None,
                }
            }
        }
    }
}
pub fn submit(
    writer: &async_channel::Sender<Write>,
    document: Document,
) -> async_channel::Receiver<Result<(), String>> {
    let (completion, receive) = async_channel::bounded(1);
    // Replaced requests lose their sender, so waiters receive cancellation.
    let _ = writer.force_send(Write {
        document,
        completion,
    });
    receive
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Document {
        let pane = |shell: &str| Pane {
            shell: Some(shell.into()),
            workspace: Some("w".into()),
        };
        Document {
            owner: "local-node".into(),
            active: "workspace:w".into(),
            arrangements: BTreeMap::from([(
                "workspace:w".into(),
                Arrangement {
                    tree: Some(Tree::Split {
                        horizontal: true,
                        ratio: 0.3,
                        first: Box::new(Tree::Pane(11)),
                        second: Box::new(Tree::Pane(22)),
                    }),
                    floating: vec![Floating {
                        pane: 33,
                        rect: [100.0, 80.0, 400.0, 300.0],
                    }],
                    panes: BTreeMap::from([
                        (11, pane("shell-a")),
                        (22, pane("remote:node-b:shell-a")),
                        (33, pane("shell-c")),
                    ]),
                    focused: Some(33),
                    expanded: Some(11),
                    canvas: [1000.0, 700.0],
                },
            )]),
            minimized: vec!["hidden-shell".into()],
            workspace_order: vec!["w".into()],
            ..Default::default()
        }
    }
    fn temporary() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("boomux-layout-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn layout_round_trip_preserves_geometry_identity_and_minimized_state() {
        let doc = fixture();
        doc.validate().unwrap();
        let decoded: Document = serde_json::from_slice(&serde_json::to_vec(&doc).unwrap()).unwrap();
        assert_eq!(doc, decoded);
    }
    #[test]
    fn layout_rejects_invalid_geometry_references_and_versions() {
        for mutate in [
            |d: &mut Document| d.version = 99,
            |d: &mut Document| d.arrangements.get_mut("workspace:w").unwrap().focused = Some(404),
            |d: &mut Document| {
                d.arrangements.get_mut("workspace:w").unwrap().floating[0].rect[2] = -1.0
            },
            |d: &mut Document| d.arrangements.get_mut("workspace:w").unwrap().floating[0].pane = 11,
            |d: &mut Document| d.arrangements.get_mut("workspace:w").unwrap().canvas[0] = f32::NAN,
            |d: &mut Document| {
                d.arrangements
                    .get_mut("workspace:w")
                    .unwrap()
                    .panes
                    .remove(&11);
            },
        ] {
            let mut doc = fixture();
            mutate(&mut doc);
            assert!(doc.validate().is_err());
        }
    }
    #[test]
    fn layout_save_is_atomic_and_prevents_stale_desktop_overwrites() {
        let root = temporary();
        let path = root.join("layout.json");
        let mut first = Store {
            path: path.clone(),
            revision: String::new(),
        };
        let mut stale = Store {
            path: path.clone(),
            revision: String::new(),
        };
        first.save(fixture()).unwrap();
        let saved = read(&path).unwrap();
        assert!(!saved.revision.is_empty());
        assert!(stale.save(Document::default()).is_err());
        assert_eq!(read(&path).unwrap(), saved);
        first.save(fixture()).unwrap();
        assert_ne!(read(&path).unwrap().revision, saved.revision);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn layout_corrupt_file_is_preserved_and_disables_writes() {
        let root = temporary();
        let path = root.join("layout.json");
        fs::write(&path, "broken").unwrap();
        let session = Session::at(Some(path.clone()));
        assert!(session.writer.is_none() && session.error.is_some());
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn layout_writer_acknowledges_durable_final_state_before_replacement_reads() {
        let root = temporary();
        let path = root.join("layout.json");
        let session = Session::at(Some(path.clone()));
        let writer = session.writer.unwrap();
        let ack = submit(&writer, fixture());
        writer.close();
        ack.recv_blocking().unwrap().unwrap();
        let restored = Session::at(Some(path));
        assert_eq!(restored.document.arrangements, fixture().arrangements);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn layout_pending_snapshots_are_coalesced_without_unbounded_queue() {
        let (writer, requests) = async_channel::bounded(1);
        let old = submit(&writer, Document::default());
        let latest = submit(&writer, fixture());
        assert!(old.recv_blocking().is_err());
        let request = requests.recv_blocking().unwrap();
        assert_eq!(request.document, fixture());
        request.completion.send_blocking(Ok(())).unwrap();
        latest.recv_blocking().unwrap().unwrap();
    }
}

#[cfg(test)]
mod unbound_tests {
    use super::*;
    #[test]
    fn empty_placeholders_collapse_without_losing_saved_shells() {
        let branch = Tree::Split {
            horizontal: true,
            ratio: 0.3,
            first: Box::new(Tree::Pane(7)),
            second: Box::new(Tree::Pane(8)),
        };
        let mut saved = Arrangement {
            tree: Some(Tree::Split {
                horizontal: false,
                ratio: 0.5,
                first: Box::new(Tree::Pane(6)),
                second: Box::new(branch.clone()),
            }),
            panes: BTreeMap::from([
                (
                    6,
                    Pane {
                        shell: None,
                        workspace: None,
                    },
                ),
                (
                    7,
                    Pane {
                        shell: Some("running".into()),
                        workspace: Some("workspace".into()),
                    },
                ),
                (
                    8,
                    Pane {
                        shell: Some("unavailable-but-reconnectable".into()),
                        workspace: None,
                    },
                ),
                (
                    9,
                    Pane {
                        shell: None,
                        workspace: None,
                    },
                ),
            ]),
            floating: vec![Floating {
                pane: 9,
                rect: [0., 0., 100., 100.],
            }],
            focused: Some(6),
            expanded: Some(9),
            canvas: [1000., 800.],
        };
        saved.discard_unbound_panes();
        assert_eq!(saved.tree, Some(branch));
        assert_eq!(saved.panes.len(), 2);
        assert!(saved.floating.is_empty());
        assert_eq!(saved.focused, Some(7));
        assert_eq!(saved.expanded, None);
        let cleaned = saved.clone();
        saved.discard_unbound_panes();
        assert_eq!(saved, cleaned);
        for pane in saved.panes.values_mut() {
            pane.shell = None;
        }
        saved.discard_unbound_panes();
        assert!(saved.tree.is_none() && saved.panes.is_empty());
        assert_eq!(saved.focused, None);
    }
}
