// Copyright 2020 The Chubao Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
// implied. See the License for the specific language governing
// permissions and limitations under the License.
use crate::pserver::simba::engine::{
    engine::{BaseEngine, Engine},
    raft::*,
    rocksdb::RocksDB,
    tantivy::Tantivy,
};
use crate::pserver::simba::latch::Latch;
use crate::pserverpb::*;
use crate::sleep;
use crate::util::{
    coding::{doc_id, id_coding},
    config::*,
    entity::*,
    error::*,
    time::current_millis,
};
use fp_rust::sync::CountDownLatch;
use log::{error, info, warn};
use prost::Message;
use serde_json::Value;
use std::cmp;
use std::marker::Send;
use std::sync::{
    atomic::{AtomicBool, Ordering::SeqCst},
    Arc, RwLock,
};

use jimraft::raft::LogReader;

pub struct Simba {
    pub conf: Arc<Config>,
    _collection: Arc<Collection>,
    pub partition: Arc<Partition>,
    readonly: bool,
    pub started: AtomicBool,
    writable: AtomicBool,
    latch: Latch,
    max_sn: RwLock<u64>,
    //engins
    pub rocksdb: Option<RocksDB>,
    pub tantivy: Option<Tantivy>,
    pub raft: Option<RaftEngine>,
    pub base_engine: Arc<BaseEngine>,
    pub server_id: u64,
    pub start_latch: Arc<Option<CountDownLatch>>,
}

impl Simba {
    pub fn new(
        conf: Arc<Config>,
        readonly: bool,
        collection: Arc<Collection>,
        partition: Arc<Partition>,
        server_id: u64,
        latch: Arc<Option<CountDownLatch>>,
    ) -> ASResult<Arc<RwLock<Simba>>> {
        let base: Arc<BaseEngine> = Arc::new(BaseEngine {
            conf: conf.clone(),
            collection: collection.clone(),
            partition: partition.clone(),
        });
        let simba: Arc<RwLock<Simba>> = Arc::new(RwLock::new(Simba {
            rocksdb: None,
            tantivy: None,
            raft: None,
            conf: conf.clone(),
            _collection: collection.clone(),
            partition: partition.clone(),
            readonly: readonly,
            started: AtomicBool::new(true),
            writable: AtomicBool::new(false),
            latch: Latch::new(50000),
            base_engine: base.clone(),
            server_id: server_id,
            start_latch: latch.clone(),
        }));

        let raft: RaftEngine = RaftEngine::new(base.clone(), simba.clone());
        simba.write().unwrap().raft = Some(raft);
        let simba_flush = simba.clone();

        tokio::spawn(async move {
            if readonly {
                return;
            }
            info!(
                "to start commit job for partition:{} begin",
                simba_flush.read().unwrap().partition.id
            );
            if let Err(e) = simba_flush.read().unwrap().flush() {
                panic!(format!(
                    "flush partition:{} has err :{}",
                    simba_flush.read().unwrap().partition.id,
                    e.to_string()
                ));
            };
            warn!(
                "parititon:{} stop commit job",
                simba_flush.read().unwrap().partition.id
            );
        });

        Ok(simba.clone())
    }

    pub fn get(&self, id: &str, sort_key: &str) -> ASResult<Vec<u8>> {
        self.get_by_iid(id_coding(id, sort_key).as_ref())
    }

    fn get_by_iid(&self, iid: &Vec<u8>) -> ASResult<Vec<u8>> {
        match self.rocksdb.as_ref().unwrap().db.get(iid) {
            Ok(ov) => match ov {
                Some(v) => Ok(v),
                None => Err(err_code_str_box(NOT_FOUND, "not found!")),
            },
            Err(e) => Err(err_box(format!("get key has err:{}", e.to_string()))),
        }
    }

    //it use 1.estimate of rocksdb  2.index of u64
    pub fn count(&self) -> ASResult<(u64, u64)> {
        let estimate_rocksdb = self.rocksdb.count()?;

        let tantivy_count = self.tantivy.count()?;

        Ok((estimate_rocksdb, tantivy_count))
    }

    pub fn search(&self, sdreq: Arc<SearchDocumentRequest>) -> SearchDocumentResponse {
        match self.tantivy.as_ref().unwrap().search(sdreq) {
            Ok(r) => r,
            Err(e) => {
                let e = cast_to_err(e);
                SearchDocumentResponse {
                    code: e.0 as i32,
                    total: 0,
                    hits: vec![],
                    info: Some(SearchInfo {
                        error: 1,
                        success: 0,
                        message: format!("search document err:{}", e.1),
                    }),
                }
            }
        }
    }

    pub fn write(&self, req: WriteDocumentRequest) -> ASResult<()> {
        let (doc, write_type) = (req.doc.unwrap(), WriteType::from_i32(req.write_type));

        match write_type {
            Some(WriteType::Overwrite) => self._overwrite(doc),
            Some(WriteType::Create) => self._overwrite(doc),
            Some(WriteType::Update) => self._update(doc),
            Some(WriteType::Upsert) => self._upsert(doc),
            Some(WriteType::Delete) => self._delete(doc),
            Some(_) | None => {
                return Err(err_box(format!("can not do the handler:{:?}", write_type)));
            }
        }
    }

    fn _create(&self, mut doc: Document) -> ASResult<()> {
        let iid = doc_id(&doc);
        doc.version = 1;
        let mut buf1 = Vec::new();
        if let Err(error) = doc.encode(&mut buf1) {
            return Err(error.into());
        }

        let _lock = self.latch.latch_lock(doc.slot);

        if let Err(e) = self.get_by_iid(&iid) {
            let e = cast_to_err(e);
            if e.0 != NOT_FOUND {
                return Err(e);
            }
        } else {
            return Err(err_box(format!("the document:{:?} already exists", iid)));
        }

        self.do_write(&iid, &buf1)
    }

    fn _update(&self, mut doc: Document) -> ASResult<()> {
        let (old_version, iid) = (doc.version, doc_id(&doc));

        let _lock = self.latch.latch_lock(doc.slot);
        let old = self.get(doc.id.as_str(), doc.sort_key.as_str())?;
        let old: Document = Message::decode(prost::bytes::Bytes::from(old))?;
        if old_version > 0 && old.version != old_version {
            return Err(err_code_box(
                VERSION_ERR,
                format!(
                    "the document:{} version not right expected:{} found:{}",
                    doc.id, old_version, old.version
                ),
            ));
        }
        merge_doc(&mut doc, old)?;
        doc.version += old_version + 1;
        let mut buf1 = Vec::new();
        if let Err(error) = doc.encode(&mut buf1) {
            return Err(error.into());
        }

        self.do_write(&iid, &buf1)
    }

    fn _upsert(&self, mut doc: Document) -> ASResult<()> {
        let iid = doc_id(&doc);
        let _lock = self.latch.latch_lock(doc.slot);
        let old = match self.get_by_iid(iid.as_ref()) {
            Ok(o) => Some(o),
            Err(e) => {
                let e = cast_to_err(e);
                if e.0 == NOT_FOUND {
                    None
                } else {
                    return Err(e);
                }
            }
        };

        if let Some(old) = old {
            let old: Document = Message::decode(prost::bytes::Bytes::from(old))?;
            doc.version = old.version + 1;
            merge_doc(&mut doc, old)?;
        } else {
            doc.version = 1;
        }

        let mut buf1 = Vec::new();
        if let Err(error) = doc.encode(&mut buf1) {
            return Err(error.into());
        }
        self.do_write(&iid, &buf1)
    }

    fn _delete(&self, doc: Document) -> ASResult<()> {
        let iid = doc_id(&doc);
        let _lock = self.latch.latch_lock(doc.slot);
        self.do_delete(&iid)
    }

    fn _overwrite(&self, mut doc: Document) -> ASResult<()> {
        let iid = doc_id(&doc);
        let mut buf1 = Vec::new();
        doc.version = 1;
        if let Err(error) = doc.encode(&mut buf1) {
            return Err(error.into());
        }
        let _lock = self.latch.latch_lock(doc.slot);
        self.do_write(&iid, &buf1)
    }

     fn do_write(&self, key: &Vec<u8>, value: &Vec<u8>) -> ASResult<()> {
        if self.check_writable() {
        self.rocksdb.write(key, value)?;
        self.tantivy.write(key, value)?;
            let latch = Arc::new(CountDownLatch::new(1));
            self.raft.as_ref().unwrap().append(
                PutEvent {
                    k: key.to_vec(),
                    v: value.to_vec(),
                },
                WriteRaftCallback {
                    latch: latch.clone(),
                },
            );
            latch.wait();
            Ok(())
        } else {
            Err(err_code_str_box(ENGINE_NOT_WRITABLE, "engin not writable!"))
        }
    }

    async fn do_delete(&self, key: &Vec<u8>) -> ASResult<()> {
        if self.check_writable() {
        self.rocksdb.delete(key)?;
        self.tantivy.delete(key)?;
            let latch = Arc::new(CountDownLatch::new(1));
            self.raft.as_ref().unwrap().append(
                DelEvent { k: key.to_vec() },
                WriteRaftCallback {
                    latch: latch.clone(),
                },
            );
            latch.wait();

            Ok(())
        } else {
            Err(err_code_str_box(ENGINE_NOT_WRITABLE, "engin not writable!"))
        }

    }

    pub fn readonly(&self) -> bool {
        return self.readonly;
    }

    fn check_writable(&self) -> bool {
        self.started.load(SeqCst) && self.writable.load(SeqCst)
    }
}

pub struct WriteRaftCallback {
    pub latch: Arc<CountDownLatch>,
}
impl AppendCallback for WriteRaftCallback {
    fn call(&self) {
        self.latch.countdown();
    }
}

impl Simba {
    fn flush(&self) -> ASResult<()> {
        let flush_time = self.conf.ps.flush_sleep_sec.unwrap_or(3) * 1000;

        let mut pre_sn = self.get_sn();

        while !self.stoped.load(SeqCst) {
            sleep!(flush_time);

            let sn = self.get_sn();

            //TODO: check pre_sn < current sn , and set

            let begin = current_millis();

            if let Err(e) = self.rocksdb.flush() {
                error!("rocksdb flush has err:{:?}", e);
            }

            if let Err(e) = self.tantivy.flush() {
                error!("rocksdb flush has err:{:?}", e);
            }

            pre_sn = sn;

            if let Err(e) = self.rocksdb.write_sn(pre_sn) {
                error!("write has err :{:?}", e);
            };

            info!("flush job ok use time:{}ms", current_millis() - begin);
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.stoped.store(true, SeqCst);
    }


    pub fn release(&self) {
        self.started.store(false, SeqCst);
        self.raft.as_ref().unwrap().release();
        self.offload_engine();
    }

    pub fn role_change(mut self, is_leader: bool) {
        if self.started.load(SeqCst) {
            let _lock = self.latch.latch_lock(u32::max_value());

            if is_leader {
                self.load_engine();
            } else {
                self.offload_engine();
            }
            if self.start_latch.is_some() {
                self.start_latch.as_ref().as_ref().unwrap().countdown();
            }
        }
    }
    fn offload_engine(&self) {
        self.writable.store(false, SeqCst);
        self.rocksdb.as_ref().unwrap().release();
        self.tantivy.as_ref().unwrap().release();
    }

    fn load_engine(&mut self) {
        let rocksdb = RocksDB::new(BaseEngine::new(&self.base_engine)).unwrap();
        let tantivy = Tantivy::new(BaseEngine::new(&self.base_engine)).unwrap();
        let log_start_index = cmp::min(rocksdb.get_sn(), tantivy.get_sn());
        self.rocksdb = Some(rocksdb);
        self.tantivy = Some(tantivy);
        match self.raft.as_ref().unwrap().begin_read_log(log_start_index) {
            Ok(logger) => {
                loop {
                    match logger.next_log() {
                        Ok((_, index, data, finished)) => {
                            let data: Vec<u8> = data.as_bytes().to_vec();
                            let log_index = index;
                            if data.len() > 0 {
                                match LogEvent::to_event(data) {
                                    Ok(event) => match event.get_type() {
                                        EventType::Delete => {
                                            let del = DelEvent {
                                                k: vec![0, 0, 0, 0],
                                            }; //event as Box<DelEvent>;
                                            self.rocksdb
                                                .as_ref()
                                                .unwrap()
                                                .delete(log_index, &del.k.clone());
                                            self.tantivy
                                                .as_ref()
                                                .unwrap()
                                                .delete(log_index, &del.k.clone());
                                        }
                                        EventType::Put => {
                                            // let put = event as Box<PutEvent>;
                                            // rocksdb.write(log_index, &put.k.clone(), &put.v.clone());
                                            // tantivy.write(log_index, &put.k.clone(), &put.v.clone());
                                        }
                                    },
                                    Err(e) => print!("error"),
                                }
                            }
                            self.writable.store(true, SeqCst);
                            if finished {
                                logger.end_read_log();
                                break;
                            }
                        }
                        Err(e) => {
                            error!("read log error:[{}]", e);
                        }
                    }
                }
            }
            Err(e) => {
                error!("begin read log error:[{}]", e);
            }
        }
    }

    pub fn get_sn(&self) -> u64 {
        *self.max_sn.read().unwrap()
    }

    pub fn set_sn_if_max(&self, sn: u64) {
        let mut v = self.max_sn.write().unwrap();
        if *v < sn {
            *v = sn;
        }
    }
}

fn merge(a: &mut Value, b: Value) {
    match (a, b) {
        (a @ &mut Value::Object(_), Value::Object(b)) => {
            let a = a.as_object_mut().unwrap();
            for (k, v) in b {
                merge(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

fn merge_doc(new: &mut Document, old: Document) -> ASResult<()> {
    let mut dist: Value = serde_json::from_slice(new.source.as_slice())?;
    let src: Value = serde_json::from_slice(old.source.as_slice())?;
    merge(&mut dist, src);
    new.source = serde_json::to_vec(&dist)?;
    new.version = old.version + 1;
    Ok(())
}
