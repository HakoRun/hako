use crate::repo::Repo;
use crate::store::FsStore;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const OBJECTS: &str = "objects";
pub const CONTAINERS: &str = "containers";
pub const SESSION: &str = "SESSION";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub container: String,
    /// Always begins with '/' and never has a trailing slash (except for "/").
    pub cwd: String,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            container: "main".into(),
            cwd: "/".into(),
        }
    }
}

pub struct State {
    workdir: PathBuf,
    store: FsStore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteTarget {
    Local(String),
    ContainersList,
    Container { name: String, path: String },
    Workspace(String),
    Peers(String),
}

impl RouteTarget {
    pub fn parse(path: &str) -> RouteTarget {
        let n = path.trim_start_matches('/');
        if n == "containers" {
            return RouteTarget::ContainersList;
        }
        if let Some(rest) = n.strip_prefix("containers/") {
            if rest.is_empty() {
                return RouteTarget::ContainersList;
            }
            return match rest.split_once('/') {
                Some((name, sub)) => RouteTarget::Container {
                    name: name.to_string(),
                    path: sub.to_string(),
                },
                None => RouteTarget::Container {
                    name: rest.to_string(),
                    path: String::new(),
                },
            };
        }
        if n == "workspace" {
            return RouteTarget::Workspace(String::new());
        }
        if let Some(rest) = n.strip_prefix("workspace/") {
            return RouteTarget::Workspace(rest.to_string());
        }
        if n == "peers" {
            return RouteTarget::Peers(String::new());
        }
        if let Some(rest) = n.strip_prefix("peers/") {
            return RouteTarget::Peers(rest.to_string());
        }
        RouteTarget::Local(n.to_string())
    }
}

impl State {
    pub fn init(workdir: &Path) -> io::Result<Self> {
        fs::create_dir_all(workdir.join(OBJECTS))?;
        fs::create_dir_all(workdir.join(CONTAINERS))?;
        let store = FsStore::new(workdir.join(OBJECTS))?;
        let s = Self {
            workdir: workdir.to_path_buf(),
            store,
        };
        if s.list_containers()?.is_empty() {
            // Create the default `hako` container and populate it with the
            // embedded toybox rootfs. This is what makes `hako is hako` land
            // in a real shell environment instead of an empty marker — the
            // workspace's identity-less identity is itself a usable Linux.
            {
                let repo = s.create_container("hako")?;
                if crate::rootfs::is_available() {
                    let root = crate::rootfs::extract_rootfs(&s.store)?;
                    repo.set_working(root)?;
                    let parents: Vec<crate::Hash> = Vec::new();
                    let ts = crate::io_util::now_secs_or_zero();
                    let commit =
                        repo.commit(root, parents, "hako", "initial rootfs (toybox)", ts)?;
                    repo.write_ref("main", commit)?;
                }
            } // drop repo before gc (it borrows from s)
              // The rootfs build does ~130 sequential cursor mutations,
              // each producing an intermediate tree root that becomes
              // unreachable as soon as the next mutation lands. GC them
              // now so a fresh workspace doesn't ship with hundreds of
              // dead objects from its own creation.
            let _ = crate::maintenance::gc(&s, false);
        }
        Ok(s)
    }

    pub fn open(workdir: &Path) -> io::Result<Self> {
        let objs = workdir.join(OBJECTS);
        if !objs.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "not a hako workspace",
            ));
        }
        let store = FsStore::new(objs)?;
        Ok(Self {
            workdir: workdir.to_path_buf(),
            store,
        })
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn store(&self) -> &FsStore {
        &self.store
    }

    pub fn create_container(&self, name: &str) -> io::Result<Repo<'_>> {
        validate_container_name(name)?;
        let cdir = self.containers_dir().join(name);
        if cdir.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "container exists",
            ));
        }
        Repo::init(&cdir, &self.store)
    }

    pub fn open_container(&self, name: &str) -> io::Result<Repo<'_>> {
        validate_container_name(name)?;
        let cdir = self.containers_dir().join(name);
        if !cdir.exists() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such container"));
        }
        Repo::open(&cdir, &self.store)
    }

    /// Returns `Ok(true)` if the container existed and was removed, `Ok(false)`
    /// if it didn't exist. Mirrors `Repo::delete_ref` so the CLI can surface a
    /// `no such container` error instead of silently claiming success.
    pub fn delete_container(&self, name: &str) -> io::Result<bool> {
        validate_container_name(name)?;
        let cdir = self.containers_dir().join(name);
        if !cdir.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(cdir)?;
        Ok(true)
    }

    pub fn list_containers(&self) -> io::Result<Vec<String>> {
        let dir = self.containers_dir();
        let mut names = Vec::new();
        if !dir.exists() {
            return Ok(names);
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(s) = entry.file_name().to_str() {
                    if !s.starts_with('.') {
                        names.push(s.to_string());
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }

    fn containers_dir(&self) -> PathBuf {
        self.workdir.join(CONTAINERS)
    }

    pub fn read_session(&self) -> io::Result<Session> {
        let p = self.workdir.join(SESSION);
        match fs::read_to_string(&p) {
            Ok(s) => parse_session(&s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Session::default()),
            Err(e) => Err(e),
        }
    }

    /// True if a SESSION file has been written (i.e., the user has run `cd`).
    /// Distinguishes "session is the default" from "session never set".
    pub fn session_path_exists(&self) -> bool {
        self.workdir.join(SESSION).exists()
    }

    pub fn write_session(&self, s: &Session) -> io::Result<()> {
        validate_container_name(&s.container)?;
        let mut out = String::new();
        out.push_str("container=");
        out.push_str(&s.container);
        out.push('\n');
        out.push_str("cwd=");
        out.push_str(&s.cwd);
        out.push('\n');
        crate::io_util::atomic_write(&self.workdir.join(SESSION), out.as_bytes())
    }
}

fn parse_session(text: &str) -> io::Result<Session> {
    let mut s = Session::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some(("container", v)) => s.container = v.to_string(),
            Some(("cwd", v)) => s.cwd = v.to_string(),
            _ => {}
        }
    }
    if s.container.is_empty() {
        s.container = "main".into();
    }
    if s.cwd.is_empty() {
        s.cwd = "/".into();
    }
    Ok(s)
}

fn validate_container_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\0')
        || name.contains("..")
        || name.starts_with('.')
        || name.starts_with('-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid container name",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_local() {
        assert_eq!(
            RouteTarget::parse("/a/b/c"),
            RouteTarget::Local("a/b/c".into())
        );
        assert_eq!(RouteTarget::parse("a/b"), RouteTarget::Local("a/b".into()));
        assert_eq!(RouteTarget::parse(""), RouteTarget::Local("".into()));
        assert_eq!(RouteTarget::parse("/"), RouteTarget::Local("".into()));
    }

    #[test]
    fn parse_containers_list() {
        assert_eq!(
            RouteTarget::parse("/containers"),
            RouteTarget::ContainersList
        );
        assert_eq!(
            RouteTarget::parse("/containers/"),
            RouteTarget::ContainersList
        );
        assert_eq!(
            RouteTarget::parse("containers"),
            RouteTarget::ContainersList
        );
    }

    #[test]
    fn parse_container_specific() {
        assert_eq!(
            RouteTarget::parse("/containers/foo"),
            RouteTarget::Container {
                name: "foo".into(),
                path: "".into()
            }
        );
        assert_eq!(
            RouteTarget::parse("/containers/foo/a/b"),
            RouteTarget::Container {
                name: "foo".into(),
                path: "a/b".into()
            }
        );
    }

    #[test]
    fn parse_workspace_and_peers() {
        assert_eq!(
            RouteTarget::parse("/workspace"),
            RouteTarget::Workspace("".into())
        );
        assert_eq!(
            RouteTarget::parse("/workspace/x"),
            RouteTarget::Workspace("x".into())
        );
        assert_eq!(RouteTarget::parse("/peers"), RouteTarget::Peers("".into()));
        assert_eq!(
            RouteTarget::parse("/peers/agent-1"),
            RouteTarget::Peers("agent-1".into())
        );
    }

    fn fresh() -> (TempDir, State) {
        let d = TempDir::new().unwrap();
        let s = State::init(d.path()).unwrap();
        (d, s)
    }

    #[test]
    fn init_creates_default_hako_container() {
        let (_d, s) = fresh();
        assert_eq!(s.list_containers().unwrap(), vec!["hako"]);
    }

    #[test]
    fn open_after_init() {
        let d = TempDir::new().unwrap();
        let _s = State::init(d.path()).unwrap();
        let s2 = State::open(d.path()).unwrap();
        assert_eq!(s2.list_containers().unwrap(), vec!["hako"]);
    }

    #[test]
    fn open_missing_errors() {
        let d = TempDir::new().unwrap();
        match State::open(d.path()) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn create_and_list_containers() {
        let (_d, s) = fresh();
        s.create_container("alpha").unwrap();
        s.create_container("beta").unwrap();
        assert_eq!(s.list_containers().unwrap(), vec!["alpha", "beta", "hako"]);
    }

    #[test]
    fn create_existing_errors() {
        let (_d, s) = fresh();
        match s.create_container("hako") {
            Ok(_) => panic!("expected error"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
        }
    }

    #[test]
    fn delete_container_works() {
        let (_d, s) = fresh();
        s.create_container("temp").unwrap();
        assert!(s.delete_container("temp").unwrap(), "removed → true");
        assert_eq!(s.list_containers().unwrap(), vec!["hako"]);
        assert!(!s.delete_container("temp").unwrap(), "missing → false");
    }

    #[test]
    fn open_nonexistent_container_errors() {
        let (_d, s) = fresh();
        match s.open_container("missing") {
            Ok(_) => panic!("expected error"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn invalid_container_name_rejected() {
        let (_d, s) = fresh();
        assert!(s.create_container("").is_err());
        assert!(s.create_container("a/b").is_err());
        assert!(s.create_container("..").is_err());
        assert!(s.create_container(".hidden").is_err());
        assert!(s.create_container("-foo").is_err());
        assert!(s.create_container("with\0null").is_err());
    }

    #[test]
    fn session_default_when_missing() {
        let (_d, s) = fresh();
        let sess = s.read_session().unwrap();
        assert_eq!(sess, Session::default());
        assert_eq!(sess.container, "main");
        assert_eq!(sess.cwd, "/");
    }

    #[test]
    fn session_roundtrip() {
        let (_d, s) = fresh();
        let want = Session {
            container: "alpha".into(),
            cwd: "/sub/dir".into(),
        };
        s.write_session(&want).unwrap();
        let got = s.read_session().unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn session_persists_across_open() {
        let d = TempDir::new().unwrap();
        {
            let s = State::init(d.path()).unwrap();
            s.create_container("alpha").unwrap();
            s.write_session(&Session {
                container: "alpha".into(),
                cwd: "/a/b".into(),
            })
            .unwrap();
        }
        let s2 = State::open(d.path()).unwrap();
        let got = s2.read_session().unwrap();
        assert_eq!(got.container, "alpha");
        assert_eq!(got.cwd, "/a/b");
    }

    #[test]
    fn session_write_rejects_bad_container() {
        let (_d, s) = fresh();
        let bad = Session {
            container: "../etc".into(),
            cwd: "/".into(),
        };
        assert!(s.write_session(&bad).is_err());
    }

    #[test]
    fn parse_session_ignores_blanks_and_comments() {
        let text = "# a comment\n\ncontainer=beta\ncwd=/x\n";
        let s = parse_session(text).unwrap();
        assert_eq!(s.container, "beta");
        assert_eq!(s.cwd, "/x");
    }

    #[test]
    fn containers_share_chunk_store() {
        // Two containers writing the same chunk should dedup.
        use crate::fs::ScopedFs;
        use crate::store::ChunkStore;
        use crate::tree::empty;

        let (_d, s) = fresh();
        let r1 = s.create_container("c1").unwrap();
        let r2 = s.create_container("c2").unwrap();
        let fs1 = ScopedFs::new(r1.store());
        let fs2 = ScopedFs::new(r2.store());

        // Write a chunk-eligible (>INLINE_THRESHOLD) blob via both.
        let big = vec![9u8; 1000];
        let _ = fs1.write_file(&empty(), "f1", &big).unwrap();
        let _ = fs2.write_file(&empty(), "f2", &big).unwrap();

        // The big content chunk should exist exactly once on disk.
        let h = crate::Hash::of(&big);
        assert!(s.store().has(&h).unwrap());
    }
}
