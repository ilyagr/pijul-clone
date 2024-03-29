use super::*;

/// An open, seekable change file.
pub struct ChangeFile {
    s: Option<zstd_seekable::Seekable<'static, OffFile>>,
    hashed: Hashed<Hunk<Option<Hash>, Local>, Author>,
    hash: Hash,
    unhashed: Option<toml::Value>,
}

struct OffFile {
    f: std::fs::File,
    start: u64,
}

unsafe impl Send for OffFile {}

impl std::io::Read for OffFile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.f.read(buf)
    }
}

impl std::io::Seek for OffFile {
    fn seek(&mut self, from: std::io::SeekFrom) -> Result<u64, std::io::Error> {
        use std::io::SeekFrom;
        let from = match from {
            SeekFrom::Start(s) => SeekFrom::Start(s + self.start),
            c => c,
        };
        self.f.seek(from)
    }
}

impl ChangeFile {
    /// Open a change file from a path.
    pub fn open(hash: Hash, path: &str) -> Result<Self, ChangeError> {
        use std::io::Read;
        let mut r = std::fs::File::open(path).map_err(|err| ChangeError::IoHash { err, hash })?;
        let mut buf = Vec::new();
        buf.resize(Change::OFFSETS_SIZE as usize, 0);
        r.read_exact(&mut buf)?;
        let offsets: Offsets = bincode::deserialize(&buf)?;
        if offsets.version != VERSION && offsets.version != VERSION_NOENC {
            return Err(ChangeError::VersionMismatch {
                got: offsets.version,
            });
        }

        buf.clear();
        buf.resize((offsets.unhashed_off - Change::OFFSETS_SIZE) as usize, 0);
        r.read_exact(&mut buf)?;
        let mut buf2 = vec![0u8; offsets.hashed_len as usize];
        let hashed: Hashed<Hunk<Option<Hash>, Local>, Author> = if offsets.version == VERSION {
            let mut s = zstd_seekable::Seekable::init_buf(&buf)?;
            s.decompress(&mut buf2, 0)?;
            trace!("deserialize current version {:?}", buf2.len());
            bincode::deserialize(&buf2)?
        } else {
            assert_eq!(offsets.version, VERSION_NOENC);
            let mut s = zstd_seekable::Seekable::init_buf(&buf)?;
            s.decompress(&mut buf2, 0)?;
            trace!("deserialize noenc {:?}", buf2.len());
            let h: Hashed<noenc::Hunk<Option<Hash>, Local>, noenc::Author> =
                bincode::deserialize(&buf2)?;
            h.into()
        };

        buf.resize((offsets.contents_off - offsets.unhashed_off) as usize, 0);
        let unhashed = if buf.is_empty() {
            None
        } else {
            r.read_exact(&mut buf)?;
            let mut s = zstd_seekable::Seekable::init_buf(&buf)?;
            buf2.resize(offsets.unhashed_len as usize, 0);
            s.decompress(&mut buf2, 0)?;
            trace!("parsing unhashed: {:?}", std::str::from_utf8(&buf2));
            serde_json::from_slice(&buf2).ok()
        };

        let m = r.metadata()?;
        let s = if offsets.contents_off >= m.len() {
            None
        } else {
            Some(zstd_seekable::Seekable::init(Box::new(OffFile {
                f: r,
                start: offsets.contents_off,
            }))?)
        };
        Ok(ChangeFile {
            s,
            hashed,
            hash,
            unhashed,
        })
    }

    pub fn has_contents(&self) -> bool {
        self.s.is_some()
    }

    /// Reads the contents at an offset into `buf`, and returns the
    /// number of bytes read. The bounds of the change's "contents"
    /// section are not checked.
    pub fn read_contents(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, ChangeError> {
        trace!("read_contents {:?} {:?}", offset, buf.len());
        if let Some(ref mut s) = self.s {
            Ok(s.decompress(buf, offset)?)
        } else {
            Err(ChangeError::MissingContents { hash: self.hash })
        }
    }

    pub fn hashed(&self) -> &Hashed<Hunk<Option<Hash>, Local>, Author> {
        &self.hashed
    }

    pub fn unhashed(&self) -> &Option<toml::Value> {
        &self.unhashed
    }
}
