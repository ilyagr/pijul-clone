use std::env::current_dir;
use std::path::PathBuf;

use pijul_config as config;

use anyhow::bail;
use libpijul::DOT_DIR;
use log::debug;

pub struct Repository {
    pub pristine: libpijul::pristine::sanakirja::Pristine,
    pub changes: libpijul::changestore::filesystem::FileSystem,
    pub working_copy: libpijul::working_copy::filesystem::FileSystem,
    pub config: config::Config,
    pub path: PathBuf,
    pub changes_dir: PathBuf,
}

pub const PRISTINE_DIR: &str = "pristine";
pub const CHANGES_DIR: &str = "changes";
pub const CONFIG_FILE: &str = "config";
const DEFAULT_IGNORE: [&[u8]; 2] = [b".git", b".DS_Store"];
// Static KV map of names for project kinds |-> elements
// that should go in the `.ignore` file by default.
const IGNORE_KINDS: &[(&[&str], &[&[u8]])] = &[
    (&["rust"], &[b"/target", b"Cargo.lock"]),
    (&["node", "nodejs"], &[b"node_modules"]),
    (&["lean"], &[b"/build"]),
];

#[cfg(unix)]
pub fn max_files() -> std::io::Result<usize> {
    let n = if let Ok((n, _)) = rlimit::getrlimit(rlimit::Resource::NOFILE) {
        (n as usize / (2 * std::thread::available_parallelism()?.get())).max(1)
    } else {
        256
    };
    debug!("max_files = {:?}", n);
    Ok(n)
}

#[cfg(not(unix))]
pub fn max_files() -> std::io::Result<usize> {
    Ok(1)
}

impl Repository {
    fn find_root_(cur: Option<PathBuf>, dot_dir: &str) -> Result<PathBuf, anyhow::Error> {
        let mut cur = if let Some(cur) = cur {
            cur
        } else {
            current_dir()?
        };
        cur.push(dot_dir);
        loop {
            debug!("{:?}", cur);
            if std::fs::metadata(&cur).is_err() {
                cur.pop();
                if cur.pop() {
                    cur.push(DOT_DIR);
                } else {
                    bail!("No Pijul repository found")
                }
            } else {
                break;
            }
        }
        Ok(cur)
    }

    pub fn find_root(cur: Option<PathBuf>) -> Result<Self, anyhow::Error> {
        Self::find_root_with_dot_dir(cur, DOT_DIR)
    }

    pub fn find_root_with_dot_dir(
        cur: Option<PathBuf>,
        dot_dir: &str,
    ) -> Result<Self, anyhow::Error> {
        let cur = Self::find_root_(cur, dot_dir)?;
        let mut pristine_dir = cur.clone();
        pristine_dir.push(PRISTINE_DIR);
        let mut changes_dir = cur.clone();
        changes_dir.push(CHANGES_DIR);
        let mut working_copy_dir = cur.clone();
        working_copy_dir.pop();
        let config_path = cur.join(CONFIG_FILE);
        let config = if let Ok(config) = std::fs::read(&config_path) {
            if let Ok(toml) = toml::from_str(&String::from_utf8(config)?) {
                toml
            } else {
                bail!("Could not read configuration file at {:?}", config_path)
            }
        } else {
            config::Config::default()
        };
        Ok(Repository {
            pristine: libpijul::pristine::sanakirja::Pristine::new(&pristine_dir.join("db"))?,
            working_copy: libpijul::working_copy::filesystem::FileSystem::from_root(
                &working_copy_dir,
            ),
            changes: libpijul::changestore::filesystem::FileSystem::from_root(
                &working_copy_dir,
                max_files()?,
            ),
            config,
            path: working_copy_dir,
            changes_dir,
        })
    }

    pub fn init(
        path: Option<std::path::PathBuf>,
        kind: Option<&str>,
        remote: Option<&str>,
    ) -> Result<Self, anyhow::Error> {
        use std::io::Write;

        let cur = if let Some(path) = path {
            path
        } else {
            current_dir()?
        };
        let pristine_dir = {
            let mut base = cur.clone();
            base.push(DOT_DIR);
            base.push(PRISTINE_DIR);
            base
        };
        if std::fs::metadata(&pristine_dir).is_err() {
            std::fs::create_dir_all(&pristine_dir)?;
            init_dot_ignore(cur.clone(), kind)?;
            init_default_config(&cur, remote)?;
            let changes_dir = {
                let mut base = cur.clone();
                base.push(DOT_DIR);
                base.push(CHANGES_DIR);
                base
            };

            let mut stderr = std::io::stderr();
            writeln!(stderr, "Repository created at {}", cur.to_string_lossy())?;

            Ok(Repository {
                pristine: libpijul::pristine::sanakirja::Pristine::new(&pristine_dir.join("db"))?,
                working_copy: libpijul::working_copy::filesystem::FileSystem::from_root(&cur),
                changes: libpijul::changestore::filesystem::FileSystem::from_root(
                    &cur,
                    max_files()?,
                ),
                config: config::Config::default(),
                path: cur,
                changes_dir,
            })
        } else {
            bail!("Already in a repository")
        }
    }

    pub fn update_config(&self) -> Result<(), anyhow::Error> {
        std::fs::write(
            self.path.join(DOT_DIR).join("config"),
            toml::to_string(&self.config)?,
        )?;
        Ok(())
    }
}

fn init_default_config(path: &std::path::Path, remote: Option<&str>) -> Result<(), anyhow::Error> {
    use std::io::Write;
    let mut path = path.join(DOT_DIR);
    path.push("config");
    if std::fs::metadata(&path).is_err() {
        let mut f = std::fs::File::create(&path)?;
        if let Some(rem) = remote {
            writeln!(f, "default_remote = {:?}", rem)?;
        }
        writeln!(f, "[hooks]\nrecord = []")?;
    }
    Ok(())
}

/// Create and populate an initial `.ignore` file for the repository.
/// The default elements are defined in the constant [`DEFAULT_IGNORE`].
fn init_dot_ignore(base_path: std::path::PathBuf, kind: Option<&str>) -> Result<(), anyhow::Error> {
    use std::io::Write;
    let dot_ignore_path = {
        let mut base = base_path.clone();
        base.push(".ignore");
        base
    };

    // Don't replace/modify an existing `.ignore` file.
    if dot_ignore_path.exists() {
        Ok(())
    } else {
        let mut dot_ignore = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(dot_ignore_path)?;

        for default_ignore in DEFAULT_IGNORE.iter() {
            dot_ignore.write_all(default_ignore)?;
            dot_ignore.write_all(b"\n")?;
        }
        ignore_specific(&mut dot_ignore, kind)
    }
}

/// if `kind` matches any of the known project kinds, add the associated
/// .ignore entries to the default `.ignore` file.
fn ignore_specific(
    dot_ignore: &mut std::fs::File,
    kind: Option<&str>,
) -> Result<(), anyhow::Error> {
    use std::io::Write;
    if let Some(kind) = kind {
        if let Ok((config, _)) = pijul_config::Global::load() {
            let ignore_kinds = config.ignore_kinds.as_ref();
            if let Some(kinds) = ignore_kinds.and_then(|x| x.get(kind)) {
                for entry in kinds.iter() {
                    writeln!(dot_ignore, "{}", entry)?;
                }
                return Ok(());
            }
        }
        let entries = IGNORE_KINDS
            .iter()
            .find(|(names, _)| names.iter().any(|x| kind.eq_ignore_ascii_case(x)))
            .into_iter()
            .flat_map(|(_, v)| v.iter());
        for entry in entries {
            dot_ignore.write_all(entry)?;
            dot_ignore.write_all(b"\n")?;
        }
    }
    Ok(())
}
