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

use crate::master::fs::{FsRetryCache, MasterFilesystem, OperationStatus};
use crate::master::load::{LoadManager, MasterLoadService};
use crate::master::MountManager;
use crate::master::{Master, MasterMetrics, RpcContext};
use curvine_common::conf::ClusterConf;
use curvine_common::error::FsError;
use curvine_common::fs::RpcCode;
use curvine_common::fs::{CurvineURI, Path};
use curvine_common::proto::*;
use curvine_common::state::{CreateFileOpts, FileStatus, HeartbeatStatus};
use curvine_common::utils::ProtoUtils;
use curvine_common::FsResult;
use orpc::err_box;
use orpc::handler::MessageHandler;
use orpc::io::net::ConnState;
use orpc::message::Message;
use orpc::runtime::Runtime;
use std::sync::Arc;

pub struct MasterHandler {
    pub(crate) fs: MasterFilesystem,
    pub(crate) retry_cache: Option<FsRetryCache>,
    pub(crate) metrics: &'static MasterMetrics,
    pub(crate) audit_logging_enabled: bool,
    pub(crate) conn_state: Option<ConnState>,
    pub(crate) load_service: Option<MasterLoadService>,
    pub(crate) mount_manager: Arc<MountManager>,
}

impl MasterHandler {
    pub fn new(
        conf: &ClusterConf,
        fs: MasterFilesystem,
        retry_cache: Option<FsRetryCache>,
        conn_state: Option<ConnState>,
        mount_manager: Arc<MountManager>,
        load_manager: Arc<LoadManager>,
        rt: Arc<Runtime>,
    ) -> Self {
        Self {
            fs,
            retry_cache,
            metrics: Master::get_metrics(),
            audit_logging_enabled: conf.master.audit_logging_enabled,
            conn_state,
            load_service: Some(MasterLoadService::new(load_manager, rt.clone())),
            mount_manager,
        }
    }

    pub fn get_req_cache(&self, id: i64) -> Option<OperationStatus> {
        if let Some(ref c) = self.retry_cache {
            c.get(&id)
        } else {
            None
        }
    }

    pub fn set_req_cache<T>(&self, id: i64, res: FsResult<T>) -> FsResult<T> {
        if let Some(ref c) = self.retry_cache {
            c.set_status(id, res.is_ok())
        }
        res
    }

    pub fn check_is_retry(&self, id: i64) -> FsResult<bool> {
        if let Some(ref c) = self.retry_cache {
            c.check_is_retry(id)
        } else {
            Ok(false)
        }
    }

    pub fn mkdir(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: MkdirRequest = ctx.parse_header()?;
        ctx.set_audit(Some(header.path.to_string()), None);

        let opts = ProtoUtils::mkdir_opts_from_pb(header.opts);
        let flag = self.fs.mkdir_with_opts(&header.path, opts)?;
        let rep_header = MkdirResponse { flag };
        ctx.response(rep_header)
    }

    fn create_file0(
        &mut self,
        req_id: i64,
        path: String,
        opts: CreateFileOpts,
    ) -> FsResult<FileStatus> {
        if self.check_is_retry(req_id)? {
            // HDFS retries return the results of the last calculation
            // Alluxio Retry will re-query the status.
            // The same solution as alluxio is adopted here. In fact, the hdfs solution is better, but rust requires an additional memory copy to achieve it.
            // Re-querying the file status may cause the request to be unidempotent.
            return self.fs.file_status(&path);
        }

        let res = self.fs.create_with_opts(&path, opts);
        self.set_req_cache(req_id, res)
    }

    pub fn retry_check_create_file(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: CreateFileRequest = ctx.parse_header()?;
        ctx.set_audit(Some(header.path.to_string()), None);

        let opts = ProtoUtils::create_opts_from_pb(header.opts);
        let status = self.create_file0(ctx.msg.req_id(), header.path, opts)?;

        let rep_header = CreateFileResponse {
            file_status: ProtoUtils::file_status_to_pb(status),
        };
        ctx.response(rep_header)
    }

    pub fn file_status(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: GetFileStatusRequest = ctx.parse_header()?;
        ctx.set_audit(Some(header.path.to_string()), None);

        let status = self.fs.file_status(header.path.as_str())?;
        let rep_header = GetFileStatusResponse {
            status: ProtoUtils::file_status_to_pb(status),
        };

        ctx.response(rep_header)
    }

    pub fn exists(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: ExistsRequest = ctx.parse_header()?;
        ctx.set_audit(Some(header.path.to_string()), None);

        let exists = self.fs.exists(&header.path)?;
        let rep_header = ExistsResponse { exists };
        ctx.response(rep_header)
    }

    pub fn delete0(&mut self, req_id: i64, header: DeleteRequest) -> FsResult<bool> {
        if self.check_is_retry(req_id)? {
            return Ok(true);
        }

        let res = self.fs.delete(&header.path, header.recursive);
        self.set_req_cache(req_id, res)
    }

    pub fn retry_check_delete(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: DeleteRequest = ctx.parse_header()?;
        ctx.set_audit(Some(header.path.to_string()), None);

        self.delete0(ctx.msg.req_id(), header)?;
        let rep_header = DeleteResponse::default();
        ctx.response(rep_header)
    }

    pub fn rename0(&mut self, req_id: i64, header: RenameRequest) -> FsResult<bool> {
        if self.check_is_retry(req_id)? {
            return Ok(true);
        }

        let res = self.fs.rename(&header.src, &header.dst);
        self.set_req_cache(req_id, res)
    }

    pub fn retry_check_rename(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: RenameRequest = ctx.parse_header()?;
        ctx.set_audit(Some(header.src.to_string()), Some(header.dst.to_string()));

        let result = self.rename0(ctx.msg.req_id(), header)?;
        let rep_header = RenameResponse { result };
        ctx.response(rep_header)
    }

    pub fn list_status(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: ListStatusRequest = ctx.parse_header()?;
        ctx.set_audit(Some(header.path.to_string()), None);

        let list = self.fs.list_status(&header.path)?;
        let res = list
            .into_iter()
            .map(ProtoUtils::file_status_to_pb)
            .collect();

        let rep_header = ListStatusResponse { statuses: res };
        ctx.response(rep_header)
    }

    // The add block internally determines whether it is a retry request.
    pub fn add_block(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let req: AddBlockRequest = ctx.parse_header()?;
        ctx.set_audit(Some(req.path.to_string()), None);

        let path = req.path;
        let client_addr = ProtoUtils::client_address_from_pb(req.client_address);
        let previous = req.previous.map(ProtoUtils::commit_block_from_pb);

        let located_block = self.fs.add_block(path, client_addr, previous, vec![])?;
        let rep_header = ProtoUtils::located_block_to_pb(located_block);
        ctx.response(rep_header)
    }

    // Complete_file internally determines whether it is a retry request.
    pub fn complete_file(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let req: CompleteFileRequest = ctx.parse_header()?;
        ctx.set_audit(Some(req.path.to_string()), None);

        let last = req.last.map(ProtoUtils::commit_block_from_pb);
        self.fs
            .complete_file(req.path, req.len, last, req.client_name)?;
        let rep_header = CompleteFileResponse::default();
        ctx.response(rep_header)
    }

    pub fn retry_check_append_file(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let req: AppendFileRequest = ctx.parse_header()?;
        let opts = ProtoUtils::create_opts_from_pb(req.opts);
        ctx.set_audit(Some(req.path.to_string()), None);

        if self.check_is_retry(ctx.msg.req_id())? {
            // @todo Currently, it is directly reported, and subsequent optimizations are made.
            return err_box!("append {} repeat request", req.path);
        }

        let res = {
            let (last_block, status) = self.fs.append_file(&req.path, opts)?;
            let rep_header = AppendFileResponse {
                file_status: ProtoUtils::file_status_to_pb(status),
                last_block: last_block.map(ProtoUtils::located_block_to_pb),
            };
            ctx.response(rep_header)
        };
        self.set_req_cache(ctx.msg.req_id(), res)
    }

    pub fn get_block_locations(&mut self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let req: GetBlockLocationsRequest = ctx.parse_header()?;
        ctx.set_audit(Some(req.path.to_string()), None);

        let blocks = self.fs.get_block_locations(req.path)?;
        let rep_header = GetBlockLocationsResponse {
            blocks: ProtoUtils::file_blocks_to_pb(blocks),
        };
        ctx.response(rep_header)
    }

    pub fn get_master_info(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let _: GetMasterInfoRequest = ctx.parse_header()?;

        let info = self.fs.master_info()?;
        let rep_header = ProtoUtils::master_info_to_pb(info);
        ctx.response(rep_header)
    }

    pub fn worker_heartbeat(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: WorkerHeartbeatRequest = ctx.parse_header()?;
        let mut wm = self.fs.worker_manager.write();

        let cmds = wm.heartbeat(
            &header.cluster_id,
            HeartbeatStatus::from(header.status),
            ProtoUtils::worker_address_from_pb(&header.address),
            ProtoUtils::storage_info_list_from_pb(header.storages),
        )?;
        drop(wm);

        let rep_header = WorkerHeartbeatResponse {
            cmds: ProtoUtils::worker_cmd_to_pb(cmds),
        };
        ctx.response(rep_header)
    }

    pub fn block_report(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: BlockReportListRequest = ctx.parse_header()?;
        let list = ProtoUtils::block_report_list_from_pb(header);
        self.fs.block_report(list)?;

        let rep_header = BlockReportListResponse::default();
        ctx.response(rep_header)
    }

    fn client_ip(&self) -> &str {
        match &self.conn_state {
            None => "",
            Some(v) => &v.remote_addr.hostname,
        }
    }

    pub fn clone_fs(&self) -> MasterFilesystem {
        self.fs.clone()
    }

    fn mount(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let request: MountRequest = ctx.parse_header()?;
        let curvine_uri = CurvineURI::new(request.curvine_path)?;
        let ufs_uri = CurvineURI::new(request.ufs_path)?;
        let mnt_opt = request.mount_options.unwrap_or_default();

        self.mount_manager
            .mount(None, &curvine_uri, &ufs_uri, &mnt_opt)?;
        let rep_header = MountResponse::default();
        ctx.response(rep_header)
    }

    fn update_mount(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let rep_header = MountResponse::default();
        ctx.response(rep_header)
    }

    fn umount(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let request: UnMountRequest = ctx.parse_header()?;
        let mnt_path = CurvineURI::new(request.curvine_path)?;
        self.mount_manager.umount(&mnt_path)?;
        let rep_header = UnMountResponse::default();
        ctx.response(rep_header)
    }

    fn get_mount_point(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let request: GetMountPointInfoRequest = ctx.parse_header()?;
        ctx.set_audit(Some(request.path.to_string()), None);

        let path = Path::from_str(request.path)?;
        let ret = self.mount_manager.get_mount_point(&path)?;
        let rep_header = GetMountPointInfoResponse { mount_point: ret };
        ctx.response(rep_header)
    }

    fn get_mount_table(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let _: GetMountTableRequest = ctx.parse_header()?;
        let rep_header = GetMountTableResponse {
            mount_points: self.mount_manager.get_mount_table()?,
        };
        ctx.response(rep_header)
    }

    fn set_attr_retry_check(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        if self.check_is_retry(ctx.msg.req_id())? {
            return ctx.response(SetAttrResponse::default());
        }

        let header: SetAttrRequest = ctx.parse_header()?;
        let opts = ProtoUtils::set_attr_opts_from_pb(header.opts);
        self.fs.set_attr(header.path, opts)?;

        ctx.response(SetAttrResponse::default())
    }

    fn symlink_retry_check(&self, ctx: &mut RpcContext<'_>) -> FsResult<Message> {
        let header: SymlinkRequest = ctx.parse_header()?;
        ctx.set_audit(
            Some(header.target.to_string()),
            Some(header.link.to_string()),
        );

        if self.check_is_retry(ctx.msg.req_id())? {
            return ctx.response(SymlinkResponse::default());
        }

        self.fs
            .symlink(&header.target, &header.link, header.force, header.mode)?;

        ctx.response(SymlinkResponse::default())
    }
}

impl MessageHandler for MasterHandler {
    type Error = FsError;

    fn handle(&mut self, msg: &Message) -> FsResult<Message> {
        let mut rpc_context = RpcContext::new(msg);
        let ctx = &mut rpc_context;
        let code = RpcCode::from(msg.code());

        // Check whether the master is active
        if !self.fs.master_monitor.is_active() {
            return Err(FsError::not_leader_master(ctx.code, self.client_ip()));
        }

        // Unified processing of all RPC requests
        let response = match code {
            // Load task related requests
            RpcCode::SubmitLoadJob
            | RpcCode::GetLoadStatus
            | RpcCode::CancelLoadJob
            | RpcCode::ReportLoadTask => {
                if let Some(ref mut load_service) = self.load_service {
                    return load_service.handle(msg);
                } else {
                    return Err(FsError::common("Load service not initialized"));
                }
            }

            // File system operation request
            RpcCode::Mkdir => self.mkdir(ctx),
            RpcCode::CreateFile => self.retry_check_create_file(ctx),
            RpcCode::AppendFile => self.retry_check_append_file(ctx),
            RpcCode::FileStatus => self.file_status(ctx),
            RpcCode::AddBlock => self.add_block(ctx),
            RpcCode::CompleteFile => self.complete_file(ctx),
            RpcCode::Exists => self.exists(ctx),
            RpcCode::Delete => self.retry_check_delete(ctx),
            RpcCode::Rename => self.retry_check_rename(ctx),
            RpcCode::ListStatus => self.list_status(ctx),
            RpcCode::GetBlockLocations => self.get_block_locations(ctx),
            RpcCode::SetAttr => self.set_attr_retry_check(ctx),
            RpcCode::Symlink => self.symlink_retry_check(ctx),

            RpcCode::Mount => self.mount(ctx),
            RpcCode::UnMount => self.umount(ctx),
            RpcCode::UpdateMount => self.update_mount(ctx),
            RpcCode::GetMountTable => self.get_mount_table(ctx),
            RpcCode::GetMountInfo => self.get_mount_point(ctx),

            // Worker related requests
            RpcCode::WorkerHeartbeat => self.worker_heartbeat(ctx),
            RpcCode::WorkerBlockReport => self.block_report(ctx),
            RpcCode::GetMasterInfo => self.get_master_info(ctx),

            // Unsupported request
            _ => err_box!("Unsupported operation"),
        };

        // Record request processing time and audit log
        let used_us = ctx.spent.used_us();
        if self.audit_logging_enabled {
            ctx.audit_log(response.is_ok(), used_us, self.conn_state.as_ref());
        }

        let code_label = format!("{:?}", ctx.code);
        self.metrics
            .rpc_request_time
            .with_label_values(&[&code_label])
            .inc_by(used_us as i64);
        self.metrics
            .rpc_request_count
            .with_label_values(&[&code_label])
            .inc();

        match response {
            Ok(v) => Ok(v),
            Err(e) => Ok(msg.error_ext(&e)),
        }
    }
}
