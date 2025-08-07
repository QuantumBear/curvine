// Copyright 2025 OPPO.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::master::fs::{MasterFilesystem, WorkerManager};
use crate::master::journal::{JournalLoader, JournalWriter};
use crate::master::meta::FsDir;
use crate::master::{MasterMonitor, MetaRaftJournal, MountManager, SyncFsDir, SyncWorkerManager};
use curvine_common::conf::ClusterConf;
use curvine_common::proto::raft::SnapshotData;
use curvine_common::raft::storage::{AppStorage, LogStorage, RocksLogStorage};
use curvine_common::raft::{RaftClient, RaftResult, RoleMonitor, RoleStateListener};
use curvine_common::FsResult;
use log::info;
use orpc::common::FileUtils;
use orpc::runtime::{RpcRuntime, Runtime};
use orpc::sync::StateCtl;
use prost::Message;
use raft::eraftpb::Entry;
use raft::Storage;
use std::sync::Arc;

// Send and replay metadata operation logs based on raft.
pub struct JournalSystem {
    rt: Arc<Runtime>,
    fs: MasterFilesystem,
    worker_manager: SyncWorkerManager,
    raft_journal: MetaRaftJournal,
    master_monitor: MasterMonitor,
    mount_manager: Arc<MountManager>,
}

impl JournalSystem {
    fn new(
        rt: Arc<Runtime>,
        fs: MasterFilesystem,
        worker_manager: SyncWorkerManager,
        raft_journal: MetaRaftJournal,
        master_monitor: MasterMonitor,
        mount_manager: Arc<MountManager>,
    ) -> Self {
        Self {
            rt,
            fs,
            worker_manager,
            raft_journal,
            master_monitor,
            mount_manager,
        }
    }

    pub fn from_conf(conf: &ClusterConf) -> FsResult<Self> {
        // When the journal system is used, please note that it is separate from the fs system.
        let rt = conf.journal.create_runtime();

        let log_store = RocksLogStorage::from_conf(&conf.journal, conf.format_master);
        let worker_manager = SyncWorkerManager::new(WorkerManager::new(conf));

        let client = RaftClient::from_conf(rt.clone(), &conf.journal);
        let journal_writer = JournalWriter::new(conf.testing, client, &conf.journal);

        // If a snapshot exists in the current system, fs_dir will be restored based on the snapshot.
        // Here we will first delete the rocksdb data directory, which is to prevent the creation of the memory directory tree twice.
        let db_conf = conf.meta_rocks_conf();
        if !conf.format_master && log_store.has_snapshot() {
            info!(
                "There is a snapshot currently, the original data directory {} will be \
            deleted and restored based on the snapshot later",
                db_conf.data_dir
            );
            FileUtils::delete_path(&db_conf.data_dir, true)?;
        }

        let role_monitor = RoleMonitor::new();
        let master_monitor = MasterMonitor::new(role_monitor.read_ctl(), StateCtl::new(0));

        let fs_dir = SyncFsDir::new(FsDir::new(conf, journal_writer)?);
        let fs = MasterFilesystem::new(
            conf,
            fs_dir.clone(),
            worker_manager.clone(),
            master_monitor.clone(),
        );

        let mount_manager = Arc::new(MountManager::new(fs.clone()));

        let raft_journal = MetaRaftJournal::new(
            rt.clone(),
            log_store,
            JournalLoader::new(fs_dir.clone(), mount_manager.clone(), &conf.journal),
            role_monitor,
            conf.journal.clone(),
        );

        let js = Self::new(
            rt,
            fs,
            worker_manager,
            raft_journal,
            master_monitor,
            mount_manager,
        );

        Ok(js)
    }

    pub async fn start(self) -> FsResult<RoleStateListener> {
        let listener = self.raft_journal.run().await?;
        Ok(listener)
    }

    pub fn start_blocking(self) -> FsResult<RoleStateListener> {
        let rt = self.rt.clone();
        let js = self;
        rt.block_on(async move { js.start().await })
    }

    pub fn fs(&self) -> MasterFilesystem {
        self.fs.clone()
    }

    pub fn worker_manager(&self) -> SyncWorkerManager {
        self.worker_manager.clone()
    }

    pub fn state_listener(&self) -> RoleStateListener {
        self.raft_journal.new_state_listener()
    }

    pub fn master_monitor(&self) -> MasterMonitor {
        self.master_monitor.clone()
    }

    pub fn mount_manager(&self) -> Arc<MountManager> {
        self.mount_manager.clone()
    }

    // Create a snapshot manually, dedicated for testing.
    pub fn create_snapshot(&self) -> RaftResult<()> {
        let data = self.raft_journal.app_store().create_snapshot(1, 1)?;

        // The test will not generate a raft log. Please modify the status here.
        let entry = Entry {
            index: 1,
            ..Default::default()
        };

        self.raft_journal.log_store().append(&[entry])?;
        self.raft_journal.log_store().set_hard_state_commit(1)?;

        self.raft_journal.log_store().create_snapshot(data, 0)
    }

    // Manually apply a snapshot, dedicated for testing.
    pub fn apply_snapshot(&self) -> RaftResult<()> {
        let snapshot = self.raft_journal.log_store().snapshot(0, 0)?;
        let data = SnapshotData::decode(snapshot.get_data())?;
        self.raft_journal.app_store().apply_snapshot(&data)
    }
}
