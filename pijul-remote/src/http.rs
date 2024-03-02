use anyhow::bail;
use libpijul::pristine::{Base32, Position};
use libpijul::Hash;
use log::{debug, error, trace};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use crate::CS;
use pijul_interaction::ProgressBar;

const USER_AGENT: &str = concat!("pijul-", env!("CARGO_PKG_VERSION"));

pub struct Http {
    pub url: url::Url,
    pub channel: String,
    pub client: reqwest::Client,
    pub name: String,
    pub headers: Vec<(String, String)>,
}

async fn download_change(
    client: reqwest::Client,
    url: url::Url,
    headers: Vec<(String, String)>,
    mut path: PathBuf,
    c: CS,
) -> Result<CS, anyhow::Error> {
    let (req, c32) = match c {
        CS::Change(c) => {
            libpijul::changestore::filesystem::push_filename(&mut path, &c);
            ("change", c.to_base32())
        }
        CS::State(c) => {
            libpijul::changestore::filesystem::push_tag_filename(&mut path, &c);
            if std::fs::metadata(&path).is_ok() {
                bail!("Tag already downloaded: {}", c.to_base32())
            }
            ("tag", c.to_base32())
        }
    };
    tokio::fs::create_dir_all(&path.parent().unwrap())
        .await
        .unwrap();
    let path_ = path.with_extension("tmp");
    let mut f = tokio::fs::File::create(&path_).await.unwrap();
    let url = format!("{}/{}", url, super::DOT_DIR);
    let mut delay = 1f64;

    let (send, mut recv) = tokio::sync::mpsc::channel::<Option<bytes::Bytes>>(100);
    let t = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        debug!("waiting chunk {:?}", c);
        while let Some(chunk) = recv.recv().await {
            match chunk {
                Some(chunk) => {
                    trace!("writing {:?}", chunk.len());
                    f.write_all(&chunk).await?;
                }
                None => {
                    f.set_len(0).await?;
                }
            }
            debug!("waiting chunk {:?}", c);
        }
        debug!("done chunk {:?}", c);
        f.flush().await?;
        Ok::<_, std::io::Error>(())
    });

    let mut done = false;
    while !done {
        let mut req = client
            .get(&url)
            .query(&[(req, &c32)])
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (k, v) in headers.iter() {
            debug!("kv = {:?} {:?}", k, v);
            req = req.header(k.as_str(), v.as_str());
        }
        let mut res = if let Ok(res) = req.send().await {
            delay = 1f64;
            res
        } else {
            debug!("HTTP error, retrying in {} seconds", delay.round());
            tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
            send.send(None).await?;
            delay *= 2.;
            continue;
        };
        debug!("response {:?}", res);
        if !res.status().is_success() {
            tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
            send.send(None).await?;
            bail!("Server returned {}", res.status().as_u16())
        }
        let mut size: Option<usize> = res
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|x| x.to_str().ok())
            .and_then(|x| x.parse().ok());
        while !done {
            match res.chunk().await {
                Ok(Some(chunk)) => {
                    if let Some(ref mut s) = size {
                        *s -= chunk.len();
                    }
                    send.send(Some(chunk)).await?;
                }
                Ok(None) => match size {
                    Some(0) | None => done = true,
                    _ => break,
                },
                Err(e) => {
                    debug!("error {:?}", e);
                    error!("Error while downloading {:?} from {:?}, retrying", c32, url);
                    send.send(None).await?;
                    tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
                    delay *= 2.;
                    break;
                }
            }
        }
    }
    std::mem::drop(send);
    t.await??;
    debug!("renaming {:?} {:?} {:?} {:?}", c, path_, path, done);
    if done {
        match c {
            CS::Change(_) => {
                tokio::fs::rename(&path_, &path).await?;
            }
            CS::State(_) => {
                tokio::fs::rename(&path_, &path).await?;
            }
        }
    }
    debug!("download_change returning {:?}", c);
    Ok(c)
}

const POOL_SIZE: usize = 20;

impl Http {
    pub async fn download_changes(
        &mut self,
        progress_bar: ProgressBar,
        hashes: &mut tokio::sync::mpsc::UnboundedReceiver<CS>,
        send: &mut tokio::sync::mpsc::Sender<(CS, bool)>,
        path: &PathBuf,
        _full: bool,
    ) -> Result<(), anyhow::Error> {
        debug!("starting download_changes http");
        let mut pool: [Option<tokio::task::JoinHandle<Result<CS, _>>>; POOL_SIZE] =
            <[_; POOL_SIZE]>::default();
        let mut cur = 0;
        loop {
            if let Some(t) = pool[cur].take() {
                debug!("waiting for process {:?}", cur);
                let c_ = t.await.unwrap().unwrap();
                debug!("sending {:?}", c_);
                progress_bar.inc(1);
                if send.send((c_, true)).await.is_err() {
                    debug!("err for {:?}", c_);
                    break;
                }
                debug!("sent {:?}", c_);
                continue;
            }
            let mut next = cur;
            for i in 1..POOL_SIZE {
                if pool[(cur + i) % POOL_SIZE].is_some() {
                    next = (cur + i) % POOL_SIZE;
                    break;
                }
            }
            if next == cur {
                if let Some(c) = hashes.recv().await {
                    debug!("downloading on process {:?}: {:?}", cur, c);
                    pool[cur] = Some(tokio::spawn(download_change(
                        self.client.clone(),
                        self.url.clone(),
                        self.headers.clone(),
                        path.clone(),
                        c,
                    )));
                    cur = (cur + 1) % POOL_SIZE;
                } else {
                    break;
                }
            } else {
                tokio::select! {
                    c = hashes.recv() => {
                        if let Some(c) = c {
                            debug!("downloading on process {:?}: {:?}", cur, c);
                            pool[cur] = Some(tokio::spawn(download_change(
                                self.client.clone(),
                                self.url.clone(),
                                self.headers.clone(),
                                path.clone(),
                                c,
                            )));
                            cur = (cur + 1) % POOL_SIZE;
                        } else {
                            break;
                        }
                    }
                    c = pool[next].as_mut().unwrap() => {
                        pool[next] = None;
                        let c = c??;
                        progress_bar.inc(1);
                        if send.send((c, true)).await.is_err() {
                            debug!("err for {:?}", c);
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn upload_changes(
        &self,
        progress_bar: ProgressBar,
        mut local: PathBuf,
        to_channel: Option<&str>,
        changes: &[CS],
    ) -> Result<(), anyhow::Error> {
        for c in changes {
            let url = {
                let mut p = self.url.path().to_string();
                if !p.ends_with("/") {
                    p.push('/')
                }
                p.push_str(super::DOT_DIR);
                let mut u = self.url.clone();
                u.set_path(&p);
                u
            };
            let mut to_channel = if let Some(ch) = to_channel {
                vec![("to_channel", ch)]
            } else {
                Vec::new()
            };
            let base32;
            let body = match c {
                CS::Change(c) => {
                    libpijul::changestore::filesystem::push_filename(&mut local, &c);
                    let change = std::fs::read(&local)?;
                    base32 = c.to_base32();
                    to_channel.push(("apply", &base32));
                    change
                }
                CS::State(c) => {
                    libpijul::changestore::filesystem::push_tag_filename(&mut local, &c);
                    let mut tag_file = libpijul::tag::OpenTagFile::open(&local, &c)?;
                    let mut v = Vec::new();
                    tag_file.short(&mut v)?;
                    base32 = c.to_base32();
                    to_channel.push(("tagup", &base32));
                    v
                }
            };
            libpijul::changestore::filesystem::pop_filename(&mut local);
            debug!("url {:?} {:?}", url, to_channel);
            let mut req = self
                .client
                .post(url)
                .query(&to_channel)
                .header(reqwest::header::USER_AGENT, USER_AGENT);
            for (k, v) in self.headers.iter() {
                debug!("kv = {:?} {:?}", k, v);
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = req.body(body).send().await?;
            let stat = resp.status();
            if !stat.is_success() {
                let body = resp.text().await?;
                if !body.is_empty() {
                    bail!("The HTTP server returned an error: {}", body)
                } else {
                    if let Some(reason) = stat.canonical_reason() {
                        bail!("HTTP Error {}: {}", stat.as_u16(), reason)
                    } else {
                        bail!("HTTP Error {}", stat.as_u16())
                    }
                }
            }
            progress_bar.inc(1);
        }
        Ok(())
    }

    pub async fn download_changelist<
        A,
        F: FnMut(&mut A, u64, Hash, libpijul::Merkle, bool) -> Result<(), anyhow::Error>,
    >(
        &self,
        mut f: F,
        a: &mut A,
        from: u64,
        paths: &[String],
    ) -> Result<HashSet<Position<Hash>>, anyhow::Error> {
        let url = {
            let mut p = self.url.path().to_string();
            if !p.ends_with("/") {
                p.push('/')
            }
            p.push_str(super::DOT_DIR);
            let mut u = self.url.clone();
            u.set_path(&p);
            u
        };
        let from_ = from.to_string();
        let mut query = vec![("changelist", &from_), ("channel", &self.channel)];
        for p in paths.iter() {
            query.push(("path", p));
        }
        let mut req = self
            .client
            .get(url)
            .query(&query)
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (k, v) in self.headers.iter() {
            debug!("kv = {:?} {:?}", k, v);
            req = req.header(k.as_str(), v.as_str());
        }
        let res = req.send().await?;
        let status = res.status();
        if !status.is_success() {
            match serde_json::from_slice::<libpijul::RemoteError>(&*res.bytes().await?) {
                Ok(remote_err) => return Err(remote_err.into()),
                Err(_) if status.as_u16() == 404 => {
                    bail!("Repository `{}` not found (404)", self.url)
                }
                Err(_) => bail!("Http request failed with status code: {}", status),
            }
        }
        let resp = res.bytes().await?;
        let mut result = HashSet::new();
        if let Ok(data) = std::str::from_utf8(&resp) {
            for l in data.lines() {
                debug!("l = {:?}", l);
                if !l.is_empty() {
                    match super::parse_line(l)? {
                        super::ListLine::Change { n, m, h, tag } => f(a, n, h, m, tag)?,
                        super::ListLine::Position(pos) => {
                            result.insert(pos);
                        }
                        super::ListLine::Error(e) => {
                            let mut stderr = std::io::stderr();
                            writeln!(stderr, "{}", e)?;
                        }
                    }
                } else {
                    break;
                }
            }
            debug!("done");
        }
        Ok(result)
    }

    pub async fn get_state(
        &mut self,
        mid: Option<u64>,
    ) -> Result<Option<(u64, libpijul::Merkle, libpijul::Merkle)>, anyhow::Error> {
        debug!("get_state {:?}", self.url);
        let url = format!("{}/{}", self.url, super::DOT_DIR);
        let q = if let Some(mid) = mid {
            [
                ("state", format!("{}", mid)),
                ("channel", self.channel.clone()),
            ]
        } else {
            [("state", String::new()), ("channel", self.channel.clone())]
        };
        let mut req = self
            .client
            .get(&url)
            .query(&q)
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (k, v) in self.headers.iter() {
            debug!("kv = {:?} {:?}", k, v);
            req = req.header(k.as_str(), v.as_str());
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            bail!("HTTP error {:?}", res.status())
        }
        let resp = res.bytes().await?;
        let resp = std::str::from_utf8(&resp)?;
        debug!("resp = {:?}", resp);
        let mut s = resp.split_whitespace();
        if let (Some(n), Some(m), Some(m2)) = (
            s.next().and_then(|s| s.parse().ok()),
            s.next()
                .and_then(|m| libpijul::Merkle::from_base32(m.as_bytes())),
            s.next()
                .and_then(|m| libpijul::Merkle::from_base32(m.as_bytes())),
        ) {
            Ok(Some((n, m, m2)))
        } else {
            Ok(None)
        }
    }

    pub async fn get_id(&self) -> Result<Option<libpijul::pristine::RemoteId>, anyhow::Error> {
        debug!("get_state {:?}", self.url);
        let url = format!("{}/{}", self.url, super::DOT_DIR);
        let q = [("channel", self.channel.clone()), ("id", String::new())];
        let mut req = self
            .client
            .get(&url)
            .query(&q)
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (k, v) in self.headers.iter() {
            debug!("kv = {:?} {:?}", k, v);
            req = req.header(k.as_str(), v.as_str());
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            bail!("HTTP error {:?}", res.status())
        }
        let resp = res.bytes().await?;
        debug!("resp = {:?}", resp);
        Ok(libpijul::pristine::RemoteId::from_bytes(&resp))
    }

    pub async fn archive<W: std::io::Write + Send + 'static>(
        &mut self,
        prefix: Option<String>,
        state: Option<(libpijul::Merkle, &[Hash])>,
        mut w: W,
    ) -> Result<u64, anyhow::Error> {
        let url = {
            let mut p = self.url.path().to_string();
            if !p.ends_with("/") {
                p.push('/')
            }
            p.push_str(super::DOT_DIR);
            let mut u = self.url.clone();
            u.set_path(&p);
            u
        };
        let res = self.client.get(url).query(&[("channel", &self.channel)]);
        let res = if let Some((ref state, ref extra)) = state {
            let mut q = vec![("archive".to_string(), state.to_base32())];
            if let Some(pre) = prefix {
                q.push(("outputPrefix".to_string(), pre));
            }
            for e in extra.iter() {
                q.push(("change".to_string(), e.to_base32()))
            }
            res.query(&q)
        } else {
            res
        };
        let res = res
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await?;
        if !res.status().is_success() {
            bail!("HTTP error {:?}", res.status())
        }
        use futures_util::StreamExt;
        let mut stream = res.bytes_stream();
        let mut conflicts = 0;
        let mut n = 0;
        while let Some(item) = stream.next().await {
            let item = item?;
            let mut off = 0;
            while n < 8 && off < item.len() {
                conflicts = (conflicts << 8) | (item[off] as u64);
                off += 1;
                n += 1
            }
            w.write_all(&item[off..])?;
        }
        Ok(conflicts as u64)
    }

    pub async fn update_identities(
        &mut self,
        rev: Option<u64>,
        mut path: PathBuf,
    ) -> Result<u64, anyhow::Error> {
        let url = {
            let mut p = self.url.path().to_string();
            if !p.ends_with("/") {
                p.push('/')
            }
            p.push_str(super::DOT_DIR);
            let mut u = self.url.clone();
            u.set_path(&p);
            u
        };
        let mut req = self
            .client
            .get(url)
            .query(&[(
                "identities",
                if let Some(rev) = rev {
                    rev.to_string()
                } else {
                    0u32.to_string()
                },
            )])
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (k, v) in self.headers.iter() {
            debug!("kv = {:?} {:?}", k, v);
            req = req.header(k.as_str(), v.as_str());
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            bail!("HTTP error {:?}", res.status())
        }
        use serde_derive::*;
        #[derive(Debug, Deserialize)]
        struct Identities {
            id: Vec<pijul_identity::Complete>,
            rev: u64,
        }
        let resp: Option<Identities> = res.json().await?;

        if let Some(resp) = resp {
            std::fs::create_dir_all(&path)?;
            for id in resp.id.iter() {
                path.push(&id.public_key.key);
                debug!("recv identity: {:?} {:?}", id, path);
                let mut id_file = std::fs::File::create(&path)?;
                serde_json::to_writer_pretty(&mut id_file, &id.as_portable())?;
                path.pop();
            }
            Ok(resp.rev)
        } else {
            Ok(0)
        }
    }

    pub async fn prove(&mut self, key: libpijul::key::SKey) -> Result<(), anyhow::Error> {
        debug!("prove {:?}", self.url);
        let url = format!("{}/{}", self.url, super::DOT_DIR);
        let q = [("challenge", key.public_key().key)];
        let mut req = self
            .client
            .get(&url)
            .query(&q)
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (k, v) in self.headers.iter() {
            debug!("kv = {:?} {:?}", k, v);
            req = req.header(k.as_str(), v.as_str());
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            bail!("HTTP error {:?}", res.status())
        }
        let resp = res.bytes().await?;
        debug!("resp = {:?}", resp);

        let sig = key.sign_raw(&resp)?;
        debug!("sig = {:?}", sig);
        let q = [("prove", &sig)];
        let mut req = self
            .client
            .get(&url)
            .query(&q)
            .header(reqwest::header::USER_AGENT, USER_AGENT);
        for (k, v) in self.headers.iter() {
            debug!("kv = {:?} {:?}", k, v);
            req = req.header(k.as_str(), v.as_str());
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            bail!("HTTP error {:?}", res.status())
        }

        Ok(())
    }
}
