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

use crate::fs::operator::*;
use crate::fs::state::NodeState;
use crate::fs::FuseFile;
use crate::raw::fuse_abi::*;
use crate::raw::FuseDirentList;
use crate::session::{FuseBuf, FuseResponse};
use crate::*;
use crate::{err_fuse, FuseError, FuseResult, FuseUtils};
use curvine_client::unified::UnifiedFileSystem;
use curvine_common::conf::{ClusterConf, FuseConf};
use curvine_common::error::FsError;
use curvine_common::fs::{FileSystem, Path};
use curvine_common::state::{FileStatus, SetAttrOpts};
use log::{debug, error, info};
use orpc::common::ByteUnit;
use orpc::runtime::Runtime;
use orpc::{sys, try_option};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::bytes::BytesMut;

pub struct CurvineFileSystem {
    fs: UnifiedFileSystem,
    state: NodeState,
    conf: FuseConf,
}

impl CurvineFileSystem {
    pub fn new(conf: ClusterConf, rt: Arc<Runtime>) -> FuseResult<Self> {
        let fuse_conf = conf.fuse.clone();
        let state = NodeState::new(&fuse_conf);
        let fs = UnifiedFileSystem::with_rt(conf, rt)?;

        let fuse_fs = Self {
            fs,
            state,
            conf: fuse_conf,
        };

        Ok(fuse_fs)
    }

    fn fill_open_flags(conf: &FuseConf, v: u32) -> u32 {
        let mut flags = v;
        if conf.direct_io {
            flags |= FUSE_FOPEN_DIRECT_IO;
        }
        if conf.kernel_cache {
            flags |= FUSE_FOPEN_KEEP_CACHE;
        }
        if conf.cache_readdir {
            flags |= FUSE_FOPEN_CACHE_DIR
        }
        if conf.non_seekable {
            flags |= FUSE_FOPEN_NONSEEKABLE
        }

        flags
    }

    pub fn conf(&self) -> &FuseConf {
        &self.conf
    }

    pub fn status_to_attr(conf: &FuseConf, status: &FileStatus) -> FuseResult<fuse_attr> {
        let blocks = (status.len as f64 / FUSE_BLOCK_SIZE as f64).ceil() as u64;
        let ctime_sec = (status.mtime / 1000) as u64;
        let ctime_nsec = ((status.mtime % 1000) * 1000) as u32;

        // Try to get uid from FileStatus owner, fallback to config
        let uid = if status.owner.is_empty() {
            conf.uid
        } else {
            // First try to parse as numeric uid
            if let Ok(numeric_uid) = status.owner.parse::<u32>() {
                numeric_uid
            } else {
                // If not numeric, try to lookup by username using system call
                match orpc::sys::get_uid_by_name(&status.owner) {
                    Some(uid) => uid,
                    None => {
                        return err_fuse!(
                            libc::EINVAL,
                            "Cannot resolve username '{}' to UID",
                            status.owner
                        );
                    }
                }
            }
        };

        // Try to get gid from FileStatus group, fallback to config
        let gid = if status.group.is_empty() {
            conf.gid
        } else {
            // First try to parse as numeric gid
            if let Ok(numeric_gid) = status.group.parse::<u32>() {
                numeric_gid
            } else {
                // If not numeric, try to lookup by group name using system call
                match orpc::sys::get_gid_by_name(&status.group) {
                    Some(gid) => gid,
                    None => {
                        return err_fuse!(
                            libc::EINVAL,
                            "Cannot resolve group name '{}' to GID",
                            status.group
                        );
                    }
                }
            }
        };

        // Use mode from FileStatus if available, otherwise use default calculation
        let mode = if status.mode != 0 {
            FuseUtils::get_mode(status.mode, status.file_type)
        } else {
            FuseUtils::get_mode(FUSE_DEFAULT_MODE & !conf.umask, status.file_type)
        };

        Ok(fuse_attr {
            ino: status.id as u64,
            size: status.len as u64,
            blocks,
            atime: 0,
            mtime: ctime_sec,
            ctime: ctime_sec,
            atimensec: 0,
            mtimensec: ctime_nsec,
            ctimensec: ctime_nsec,
            mode,
            nlink: 1,
            uid,
            gid,
            rdev: 0,
            blksize: FUSE_BLOCK_SIZE as u32,
            padding: 0,
        })
    }

    pub fn create_entry_out(conf: &FuseConf, attr: fuse_attr) -> fuse_entry_out {
        fuse_entry_out {
            nodeid: attr.ino,
            generation: 0,
            entry_valid: conf.entry_ttl.as_secs(),
            attr_valid: conf.attr_ttl.as_secs(),
            entry_valid_nsec: conf.entry_ttl.subsec_nanos(),
            attr_valid_nsec: conf.attr_ttl.subsec_nanos(),
            attr,
        }
    }

    fn new_dot_status(name: &str) -> FileStatus {
        FileStatus::with_name(FUSE_UNKNOWN_INO as i64, name.to_string(), true)
    }

    async fn fs_list_status(&self, parent: u64, path: &Path) -> FuseResult<Vec<FileStatus>> {
        let mut res = vec![];
        res.push(Self::new_dot_status(FUSE_CURRENT_DIR));
        res.push(Self::new_dot_status(FUSE_PARENT_DIR));

        let list = self.fs.list_status(path).await?;
        let list = if self.conf.read_dir_fill_ino {
            self.state.fill_ino(parent, list)?
        } else {
            list
        };

        for status in list {
            res.push(status);
        }

        Ok(res)
    }

    async fn fs_get_status(&self, path: &Path) -> FuseResult<FileStatus> {
        let status = match self.fs.get_status(path).await {
            Ok(v) => v,
            Err(e) => {
                return match e {
                    FsError::FileNotFound(_) => err_fuse!(libc::ENOENT, "{}", e),
                    _ => err_fuse!(libc::ENOSYS, "{}", e),
                }
            }
        };
        Ok(status)
    }

    // fuse.c peer implementation of lookup_path function.
    async fn lookup_path<T: AsRef<str>>(
        &self,
        parent: u64,
        name: Option<T>,
        path: &Path,
    ) -> FuseResult<fuse_attr> {
        let status = self.fs_get_status(path).await?;
        self.state.do_lookup(parent, name, &status)
    }

    fn lookup_status<T: AsRef<str>>(
        &self,
        parent: u64,
        name: Option<T>,
        status: &FileStatus,
    ) -> FuseResult<fuse_attr> {
        self.state.do_lookup(parent, name, status)
    }

    async fn read_dir_common(
        &self,
        header: &fuse_in_header,
        arg: &fuse_read_in,
        plus: bool,
    ) -> FuseResult<FuseDirentList> {
        let path = self.state.get_path(header.nodeid)?;
        let list = self.fs_list_status(header.nodeid, &path).await?;

        let start_index = arg.offset as usize;
        let mut res = FuseDirentList::new(arg);
        for (index, status) in list.iter().enumerate().skip(start_index) {
            if plus {
                let attr = Self::status_to_attr(&self.conf, status)?;
                let entry = Self::create_entry_out(&self.conf, attr);
                if !res.add_plus((index + 1) as u64, status, entry) {
                    break;
                }
            } else if !res.add((index + 1) as u64, status) {
                break;
            }
        }
        Ok(res)
    }

    /// Check if the current user has the requested access permissions
    fn check_access_permissions(
        &self,
        status: &FileStatus,
        current_uid: u32,
        current_gid: u32,
        mask: u32,
    ) -> bool {
        let file_uid = self.resolve_file_uid(&status.owner);
        let file_gid = self.resolve_file_gid(&status.group);
        let permission_bits = self.get_effective_permission_bits(
            status.mode,
            current_uid,
            current_gid,
            file_uid,
            file_gid,
        );

        debug!(
            "Access check: file_uid={}, file_gid={}, current_uid={}, current_gid={}, mode={:o}, permission_bits={:o}, mask={:o}",
            file_uid, file_gid, current_uid, current_gid, status.mode, permission_bits, mask
        );

        let has_permission = self.check_permission_mask(permission_bits, mask);
        debug!("Final access result: {}", has_permission);
        has_permission
    }

    /// Resolve file owner UID from string (supports both numeric and username)
    fn resolve_file_uid(&self, owner: &str) -> u32 {
        if owner.is_empty() {
            return self.conf.uid;
        }

        // Try to parse as numeric uid first
        if let Ok(numeric_uid) = owner.parse::<u32>() {
            return numeric_uid;
        }

        // If not numeric, try to lookup by username
        match orpc::sys::get_uid_by_name(owner) {
            Some(uid) => uid,
            None => {
                debug!(
                    "Failed to resolve username '{}', using fallback UID {}",
                    owner, self.conf.uid
                );
                self.conf.uid // Fallback to config uid
            }
        }
    }

    /// Resolve file group GID from string (supports both numeric and group name)
    fn resolve_file_gid(&self, group: &str) -> u32 {
        if group.is_empty() {
            return self.conf.gid;
        }

        // Try to parse as numeric gid first
        if let Ok(numeric_gid) = group.parse::<u32>() {
            return numeric_gid;
        }

        // If not numeric, try to lookup by group name
        match orpc::sys::get_gid_by_name(group) {
            Some(gid) => gid,
            None => {
                debug!(
                    "Failed to resolve group '{}', using fallback GID {}",
                    group, self.conf.gid
                );
                self.conf.gid // Fallback to config gid
            }
        }
    }

    /// Determine which permission bits to check based on user relationship to file
    fn get_effective_permission_bits(
        &self,
        mode: u32,
        current_uid: u32,
        current_gid: u32,
        file_uid: u32,
        file_gid: u32,
    ) -> u32 {
        if current_uid == file_uid {
            // Owner permissions (bits 8-10)
            (mode >> 6) & 0o7
        } else if current_gid == file_gid {
            // Group permissions (bits 5-7)
            (mode >> 3) & 0o7
        } else {
            // Other permissions (bits 2-4)
            mode & 0o7
        }
    }

    /// Check if the permission bits satisfy the requested access mask
    fn check_permission_mask(&self, permission_bits: u32, mask: u32) -> bool {
        let mut has_permission = true;

        // Check read permission (R_OK = 4)
        if (mask & libc::R_OK as u32) != 0 {
            let has_read = (permission_bits & 0o4) != 0;
            has_permission = has_permission && has_read;
            debug!(
                "Read permission check: requested=true, granted={}",
                has_read
            );
        }

        // Check write permission (W_OK = 2)
        if (mask & libc::W_OK as u32) != 0 {
            let has_write = (permission_bits & 0o2) != 0;
            has_permission = has_permission && has_write;
            debug!(
                "Write permission check: requested=true, granted={}",
                has_write
            );
        }

        // Check execute permission (X_OK = 1)
        if (mask & libc::X_OK as u32) != 0 {
            let has_execute = (permission_bits & 0o1) != 0;
            has_permission = has_permission && has_execute;
            debug!(
                "Execute permission check: requested=true, granted={}",
                has_execute
            );
        }

        has_permission
    }

    fn fuse_setattr_to_opts(setattr: &fuse_setattr_in) -> FuseResult<SetAttrOpts> {
        let owner = {
            // Try to get username from uid, fail if not found
            match orpc::sys::get_username_by_uid(setattr.uid) {
                Some(username) => Some(username),
                None => {
                    return err_fuse!(
                        libc::EINVAL,
                        "Cannot resolve UID {} to username",
                        setattr.uid
                    );
                }
            }
        };

        let group = {
            // Try to get group name from gid, fail if not found
            match orpc::sys::get_groupname_by_gid(setattr.gid) {
                Some(groupname) => Some(groupname),
                None => {
                    return err_fuse!(
                        libc::EINVAL,
                        "Cannot resolve GID {} to group name",
                        setattr.gid
                    );
                }
            }
        };

        let mode = if setattr.mode != 0 {
            Some(setattr.mode)
        } else {
            None
        };

        Ok(SetAttrOpts {
            recursive: false,
            replicas: None,
            owner,
            group,
            mode,
            ttl_ms: None,
            ttl_action: None,
            add_x_attr: HashMap::new(),
            remove_x_attr: Vec::new(),
        })
    }
}

impl fs::FileSystem for CurvineFileSystem {
    async fn init(&self, op: Init<'_>) -> FuseResult<fuse_init_out> {
        if op.arg.major < FUSE_KERNEL_VERSION && op.arg.minor < FUSE_KERNEL_MINOR_VERSION {
            return err_fuse!(
                libc::EPROTO,
                "Unsupported FUSE ABI version {}.{}",
                op.arg.major,
                op.arg.minor
            );
        }

        let mut out_flags = FUSE_BIG_WRITES
            | FUSE_ASYNC_READ
            | FUSE_ASYNC_DIO
            | FUSE_SPLICE_MOVE
            | FUSE_SPLICE_WRITE;
        let max_write = FuseUtils::get_fuse_buf_size() - FUSE_BUFFER_HEADER_SIZE;
        let page_size = sys::get_pagesize()?;
        let max_pages = if op.arg.flags & FUSE_MAX_PAGES != 0 {
            out_flags |= FUSE_MAX_PAGES;
            (max_write - 1) / page_size + 1
        } else {
            0
        };

        let out = fuse_init_out {
            major: op.arg.major,
            minor: op.arg.minor,
            max_readahead: op.arg.max_readahead,
            flags: op.arg.flags | out_flags,
            max_background: self.conf.max_background,
            congestion_threshold: self.conf.congestion_threshold,
            max_write: max_write as u32,
            #[cfg(feature = "fuse3")]
            time_gran: 1,
            #[cfg(feature = "fuse3")]
            max_pages: max_pages as u16,
            #[cfg(feature = "fuse3")]
            padding: 0,
            #[cfg(feature = "fuse3")]
            unused: 0,
        };

        Ok(out)
    }

    // Query inode.
    async fn lookup(&self, op: Lookup<'_>) -> FuseResult<fuse_entry_out> {
        let name = try_option!(op.name.to_str());
        let id = op.header.nodeid;

        let (parent, name) = if name == FUSE_CURRENT_DIR {
            (id, None)
        } else if name == FUSE_PARENT_DIR {
            let parent = self.state.get_parent_id(id)?;
            (parent, None)
        } else {
            (id, Some(name))
        };

        // Get the path.
        let path = self.state.get_path_common(parent, name)?;
        let res = self.lookup_path(parent, name, &path).await;

        let entry = match res {
            Ok(attr) => Self::create_entry_out(&self.conf, attr),

            Err(e) if e.errno == libc::ENOENT && !self.conf.negative_ttl.is_zero() => {
                fuse_entry_out {
                    entry_valid: self.conf.negative_ttl.as_secs(),
                    entry_valid_nsec: self.conf.negative_ttl.subsec_nanos(),
                    ..Default::default()
                }
            }

            Err(e) => return Err(e),
        };

        Ok(entry)
    }

    // getfattr /curvine-fuse/x.log -n id
    // Query the inode id of node in curvine system, and it is useful to troubleshoot problems.
    // Output:
    // # file: curvine-fuse/x.log
    // id="1057"
    async fn get_xattr(&self, op: GetXAttr<'_>) -> FuseResult<BytesMut> {
        let name = try_option!(op.name.to_str());
        let path = self.state.get_path(op.header.nodeid)?;

        debug!("Getting xattr: path='{}' name='{}'", path, name);

        let status = match self.fs.get_status(&path).await {
            Ok(status) => status,
            Err(e) => {
                error!("Failed to get status for {}: {}", path, e);
                return err_fuse!(libc::ENOENT, "File not found: {}", path);
            }
        };

        let mut buf = FuseBuf::default();
        match name {
            "id" => {
                let value = status.id.to_string();
                if op.arg.size == 0 {
                    buf.add_xattr_out(value.len())
                } else {
                    buf.add_slice(value.as_bytes());
                }
            }
            "security.capability" => {
                debug!("Querying file capabilities for: {}", path);
                // Return ENODATA silently to avoid ERROR logs
                let err = FuseError::new(libc::ENODATA, "No capabilities set".into());
                return Err(err);
            }
            "security.selinux" => {
                debug!("Querying SELinux context for: {}", path);
                // Return ENODATA silently to avoid ERROR logs
                let err = FuseError::new(libc::ENODATA, "No SELinux context set".into());
                return Err(err);
            }
            "system.posix_acl_access" | "system.posix_acl_default" => {
                debug!("Querying POSIX ACL for: {}", path);
                // Return ENODATA silently to avoid ERROR logs
                let err = FuseError::new(libc::ENODATA, "POSIX ACLs not supported".into());
                return Err(err);
            }
            _ => {
                // For other xattr names, try to get from file's xattr
                if let Some(value) = status.x_attr.get(name) {
                    if op.arg.size == 0 {
                        buf.add_xattr_out(value.len())
                    } else if op.arg.size < value.len() as u32 {
                        return err_fuse!(
                            libc::ERANGE,
                            "Buffer too small for xattr value: {} < {}",
                            op.arg.size,
                            value.len()
                        );
                    } else {
                        buf.add_slice(value);
                    }
                } else {
                    return err_fuse!(libc::ENODATA, "No such attribute: {}", name);
                }
            }
        }

        Ok(buf.take())
    }

    // setfattr -n system.posix_acl_access -v "user::rw-,group::r--,other::r--" /curvine-fuse/file
    // Set POSIX ACL attributes for files and directories
    async fn set_xattr(&self, op: SetXAttr<'_>) -> FuseResult<()> {
        let name = try_option!(op.name.to_str());
        let path = self.state.get_path(op.header.nodeid)?;

        // Get the xattr value from the request
        let value_slice: &[u8] = op.value;

        info!(
            "Setting xattr: path='{}' name='{}' value='{}'",
            path,
            name,
            String::from_utf8_lossy(value_slice)
        );

        // Create SetAttrOpts with the xattr to add
        let mut add_x_attr = HashMap::new();
        add_x_attr.insert(name.to_string(), value_slice.to_vec());

        let opts = SetAttrOpts {
            recursive: false,
            replicas: None,
            owner: None,
            group: None,
            mode: None,
            ttl_ms: None,
            ttl_action: None,
            add_x_attr,
            remove_x_attr: Vec::new(),
        };

        // Call backend filesystem to set the xattr
        match self.fs.set_attr(&path, opts).await {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Failed to set xattr: {}", e);
                err_fuse!(libc::EIO, "Failed to set xattr: {}", e)
            }
        }
    }

    // setfattr -x system.posix_acl_access /curvine-fuse/file
    // Remove POSIX ACL attributes from files and directories
    async fn remove_xattr(&self, op: RemoveXAttr<'_>) -> FuseResult<()> {
        let name = try_option!(op.name.to_str());
        let path = self.state.get_path(op.header.nodeid)?;

        info!("Removing xattr: path='{}' name='{}'", path, name);

        // Handle system extended attributes silently to avoid ERROR logs
        match name {
            "security.capability" => {
                debug!("Removing file capabilities for: {}", path);
                // Return success silently to avoid ERROR logs
                return Ok(());
            }
            "security.selinux" => {
                debug!("Removing SELinux context for: {}", path);
                // Return success silently to avoid ERROR logs
                return Ok(());
            }
            "system.posix_acl_access" | "system.posix_acl_default" => {
                debug!("Removing POSIX ACL for: {}", path);
                // Return success silently to avoid ERROR logs
                return Ok(());
            }
            _ => {
                // Handle user-defined attributes and other attributes
            }
        }

        // Create SetAttrOpts with the xattr to remove
        let opts = SetAttrOpts {
            recursive: false,
            replicas: None,
            owner: None,
            group: None,
            mode: None,
            ttl_ms: None,
            ttl_action: None,
            add_x_attr: HashMap::new(),
            remove_x_attr: vec![name.to_string()],
        };

        // Call backend filesystem to remove the xattr
        match self.fs.set_attr(&path, opts).await {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Failed to remove xattr: {}", e);
                err_fuse!(libc::EIO, "Failed to remove xattr: {}", e)
            }
        }
    }

    // listxattr /curvine-fuse/file
    // List all extended attributes for a file or directory
    async fn list_xattr(&self, op: ListXAttr<'_>) -> FuseResult<BytesMut> {
        let path = self.state.get_path(op.header.nodeid)?;
        debug!("Listing xattrs: path='{}' size={}", path, op.arg.size);

        let status = match self.fs.get_status(&path).await {
            Ok(status) => status,
            Err(e) => {
                error!("Failed to get status for {}: {}", path, e);
                return err_fuse!(libc::ENOENT, "File not found: {}", path);
            }
        };

        // Build the list of xattr names
        let mut xattr_names = Vec::new();

        // Add custom xattr names from the file
        for name in status.x_attr.keys() {
            xattr_names.extend_from_slice(name.as_bytes());
            xattr_names.push(0); // null terminator
        }

        // Add the special "id" attribute
        xattr_names.extend_from_slice(b"id\0");

        let mut buf = FuseBuf::default();

        // If size is 0, just return the total size needed
        if op.arg.size == 0 {
            buf.add_xattr_out(xattr_names.len());
        } else {
            // Check if the provided buffer is large enough
            if op.arg.size < xattr_names.len() as u32 {
                return err_fuse!(
                    libc::ERANGE,
                    "Buffer too small: {} < {}",
                    op.arg.size,
                    xattr_names.len()
                );
            }
            // Return the actual xattr names data
            buf.add_slice(&xattr_names);
        }

        Ok(buf.take())
    }

    // Get the attribute of the specified inode.
    async fn get_attr(&self, op: GetAttr<'_>) -> FuseResult<fuse_attr_out> {
        let path = self.state.get_path(op.header.nodeid)?;
        let attr = self
            .lookup_path::<String>(op.header.nodeid, None, &path)
            .await?;

        let attr = fuse_attr_out {
            attr_valid: self.conf.attr_ttl.as_secs(),
            attr_valid_nsec: self.conf.attr_ttl.subsec_nanos(),
            dummy: 0,
            attr,
        };
        Ok(attr)
    }

    // Modify properties
    //The chown, chmod, and truncate commands will access the interface.
    // @todo is not implemented at this time, and this interface will not cause inode to be familiar with.
    async fn set_attr(&self, op: SetAttr<'_>) -> FuseResult<fuse_attr_out> {
        info!(
            "Setting attr: path='{}', opts={:?}",
            op.header.nodeid, op.arg
        );
        let path = self.state.get_path(op.header.nodeid)?;
        let status = self.fs_get_status(&path).await?;
        let attr = self.lookup_status::<String>(op.header.nodeid, None, &status)?;

        let opts = Self::fuse_setattr_to_opts(op.arg)?;
        self.fs.set_attr(&path, opts).await?;

        let attr = fuse_attr_out {
            attr_valid: self.conf.attr_ttl.as_secs(),
            attr_valid_nsec: self.conf.attr_ttl.subsec_nanos(),
            dummy: 0,
            attr,
        };
        Ok(attr)
    }

    // This interface is not supported at present
    async fn access(&self, op: Access<'_>) -> FuseResult<()> {
        let path = self.state.get_path(op.header.nodeid)?;

        // Get file status to check permissions
        let status = match self.fs.get_status(&path).await {
            Ok(status) => status,
            Err(e) => {
                error!("Failed to get status for {}: {}", path, e);
                return err_fuse!(libc::ENOENT, "File not found: {}", path);
            }
        };

        // Get current user's UID and GID from the request header
        let current_uid = op.header.uid;
        let current_gid = op.header.gid;

        // Get requested access mask
        let mask = op.arg.mask;

        // Check if user has the requested permissions
        if !self.check_access_permissions(&status, current_uid, current_gid, mask) {
            return err_fuse!(libc::EACCES, "Permission denied for {}", path);
        }

        Ok(())
    }

    // Open the directory.
    async fn open_dir(&self, op: OpenDir<'_>) -> FuseResult<fuse_open_out> {
        let _ = OpenAction::try_from(op.arg.flags)?;
        let _ = self.state.get_node(op.header.nodeid)?;

        let fh = self.state.next_handle();
        let open_flags = Self::fill_open_flags(&self.conf, op.arg.flags);
        let attr = fuse_open_out {
            fh,
            open_flags,
            padding: 0,
        };

        Ok(attr)
    }

    // Get file system profile information.
    async fn stat_fs(&self, _: StatFs<'_>) -> FuseResult<fuse_kstatfs> {
        let info = self.fs.get_master_info().await?;

        let block_size = 4 * ByteUnit::KB as u32;
        let total_blocks = (info.capacity / block_size as i64) as u64;
        let free_blocks = (info.available / block_size as i64) as u64;

        let res = fuse_kstatfs {
            blocks: total_blocks,
            bfree: free_blocks,
            bavail: free_blocks,
            files: FUSE_UNKNOWN_INODES,
            ffree: FUSE_UNKNOWN_INODES,
            bsize: block_size,
            namelen: FUSE_MAX_NAME_LENGTH as u32,
            frsize: block_size,
            padding: 0,
            spare: [0; 6],
        };

        Ok(res)
    }

    // Create a directory.
    async fn mkdir(&self, op: MkDir<'_>) -> FuseResult<fuse_entry_out> {
        let name = try_option!(op.name.to_str());
        let path = self.state.get_path_name(op.header.nodeid, name)?;

        let _ = self.fs.mkdir(&path, false).await?;
        let entry = self
            .lookup_path(op.header.nodeid, Some(name), &path)
            .await?;
        Ok(Self::create_entry_out(&self.conf, entry))
    }

    // The kernel requests to allocate space.Not currently implemented, and in distributed systems, it is not necessary.
    async fn fuse_allocate(&self, op: FAllocate<'_>) -> FuseResult<()> {
        let _ = self.state.get_path(op.header.nodeid)?;
        Ok(())
    }

    // Release the directory, curvine does not need to implement this interface
    async fn release_dir(&self, op: ReleaseDir<'_>) -> FuseResult<()> {
        let _ = self.state.get_path(op.header.nodeid)?;
        Ok(())
    }

    async fn read_dir(&self, op: ReadDir<'_>) -> FuseResult<FuseDirentList> {
        self.read_dir_common(op.header, op.arg, false).await
    }

    async fn read_dir_plus(&self, op: ReadDirPlus<'_>) -> FuseResult<FuseDirentList> {
        self.read_dir_common(op.header, op.arg, true).await
    }

    async fn read(&self, op: Read<'_>, rep: FuseResponse) -> FuseResult<()> {
        let file = self.state.get_file_check(op.arg.fh)?;
        file.as_mut().read(op, rep).await?;
        Ok(())
    }

    async fn open(&self, op: Open<'_>) -> FuseResult<fuse_open_out> {
        let id = op.header.nodeid;
        let path = self.state.get_path(id)?;

        let file = FuseFile::create(self.fs.clone(), path, op.arg.flags).await?;
        let fh = self.state.add_file(file)?;

        let open_flags = Self::fill_open_flags(&self.conf, op.arg.flags);
        let entry = fuse_open_out {
            fh,
            open_flags,
            padding: 0,
        };

        Ok(entry)
    }

    async fn create(&self, op: Create<'_>) -> FuseResult<fuse_create_out> {
        if !FuseUtils::s_isreg(op.arg.mode) {
            return err_fuse!(libc::EIO);
        }

        let id = op.header.nodeid;
        let name = try_option!(op.name.to_str());
        if name.len() > FUSE_MAX_NAME_LENGTH {
            return err_fuse!(libc::ENAMETOOLONG);
        }

        // step1: Create a file.
        let path = self.state.get_path_common(id, Some(name))?;
        let file = FuseFile::for_write(self.fs.clone(), path, op.arg.flags).await?;
        let status = file.status()?;
        let attr = self.lookup_status(id, Some(name), status)?;

        // step2: cache file handle.
        let fh = self.state.add_file(file)?;

        let open_flags = Self::fill_open_flags(&self.conf, op.arg.flags);
        let r = fuse_create_out(
            fuse_entry_out {
                nodeid: attr.ino,
                generation: 0,
                entry_valid: self.conf.entry_ttl.as_secs(),
                attr_valid: self.conf.attr_ttl.as_secs(),
                entry_valid_nsec: self.conf.entry_ttl.subsec_nanos(),
                attr_valid_nsec: self.conf.attr_ttl.subsec_nanos(),
                attr,
            },
            fuse_open_out {
                fh,
                open_flags,
                padding: 0,
            },
        );

        Ok(r)
    }

    async fn write(&self, op: Write<'_>, reply: FuseResponse) -> FuseResult<()> {
        let file = self.state.get_file_check(op.arg.fh)?;
        file.as_mut().write(op, reply).await?;
        Ok(())
    }

    async fn flush(&self, op: Flush<'_>) -> FuseResult<()> {
        if let Some(file) = self.state.get_file(op.arg.fh) {
            file.as_mut().flush().await
        } else {
            let path = self.state.get_path(op.header.nodeid)?;
            error!("Failed to flush {}: Cannot find fh {}", path, op.arg.fh);
            Ok(())
        }
    }

    async fn release(&self, op: Release<'_>) -> FuseResult<()> {
        if let Some(file) = self.state.remove_file(op.arg.fh) {
            file.as_mut().complete().await
        } else {
            let path = self.state.get_path(op.header.nodeid)?;
            error!("Failed to release {}: Cannot find fh {}", path, op.arg.fh);
            Ok(())
        }
    }

    async fn forget(&self, op: Forget<'_>) -> FuseResult<()> {
        self.state.forget_node(op.header.nodeid, op.arg.nlookup)
    }

    async fn unlink(&self, op: Unlink<'_>) -> FuseResult<()> {
        let name = try_option!(op.name.to_str());
        let path = self.state.get_path_common(op.header.nodeid, Some(name))?;
        self.fs.delete(&path, false).await?;
        Ok(())
    }

    async fn rm_dir(&self, op: RmDir<'_>) -> FuseResult<()> {
        let name = try_option!(op.name.to_str());
        let path = self.state.get_path_common(op.header.nodeid, Some(name))?;
        self.fs.delete(&path, false).await?;
        Ok(())
    }

    async fn rename(&self, op: Rename<'_>) -> FuseResult<()> {
        let old_name = try_option!(op.old_name.to_str());
        let new_name = try_option!(op.new_name.to_str());
        if new_name.len() > FUSE_MAX_NAME_LENGTH {
            return err_fuse!(libc::ENAMETOOLONG);
        }

        let (old_path, new_path) =
            self.state
                .get_path2(op.header.nodeid, old_name, op.arg.newdir, new_name)?;
        self.fs.rename(&old_path, &new_path).await?;
        self.state
            .rename_node(op.header.nodeid, old_name, op.arg.newdir, new_name)?;

        Ok(())
    }

    // interrupt request, curvine each future has a timeout time and will not block for a long time, so this interface is not required.
    async fn interrupt(&self, _: Interrupt<'_>) -> FuseResult<()> {
        Ok(())
    }

    async fn batch_forget(&self, op: BatchForget<'_>) -> FuseResult<()> {
        self.state.batch_forget_node(op.nodes)
    }

    // Create a symbolic link
    async fn symlink(&self, op: Symlink<'_>) -> FuseResult<fuse_entry_out> {
        let linkname = try_option!(op.linkname.to_str());
        let target = try_option!(op.target.to_str());
        let id = op.header.nodeid;

        if linkname.len() > FUSE_MAX_NAME_LENGTH {
            return err_fuse!(libc::ENAMETOOLONG);
        }

        let (parent, linkname) = if linkname == FUSE_CURRENT_DIR {
            (id, None)
        } else if linkname == FUSE_PARENT_DIR {
            let parent = self.state.get_parent_id(id)?;
            (parent, None)
        } else {
            (id, Some(linkname))
        };

        let link_path = self.state.get_path_common(parent, linkname)?;

        info!(
            "symlink: linkname={:?}, target={:?}, link_path={:?}",
            linkname, target, link_path
        );
        // Call backend filesystem to create the symbolic link
        // Use force=false to prevent overwriting existing files (standard ln -s behavior)
        self.fs.symlink(target, &link_path, false).await?;

        // Get the created symlink's attributes
        let entry = self.lookup_path(parent, linkname, &link_path).await?;

        Ok(Self::create_entry_out(&self.conf, entry))
    }

    // Read the target of a symbolic link
    async fn readlink(&self, op: Readlink<'_>) -> FuseResult<BytesMut> {
        info!("readlink: nodeid={:?}", op.header.nodeid);
        let path = self.state.get_path(op.header.nodeid)?;

        // Get file status to read the symlink target
        let status = self.fs_get_status(&path).await?;

        // Check if it's actually a symlink
        if status.file_type != curvine_common::state::FileType::Link {
            return err_fuse!(libc::EINVAL, "Not a symbolic link: {}", path);
        }

        // Get the target from the file status
        let curvine_target = match status.target {
            Some(target) => target,
            None => {
                return err_fuse!(libc::ENODATA, "Symbolic link has no target: {}", path);
            }
        };

        // Return the original target path as stored (POSIX standard behavior)
        info!("readlink: path={:?}, target={:?}", path, curvine_target);

        // Return the target path as bytes
        let mut buf = crate::session::FuseBuf::default();
        let target_bytes = curvine_target.as_bytes();
        info!(
            "readlink: target_bytes={:?}, len={}",
            target_bytes,
            target_bytes.len()
        );
        buf.add_slice(target_bytes);
        let result = buf.take();
        info!(
            "readlink: result_bytes={:?}, len={}",
            result.as_ref(),
            result.len()
        );
        Ok(result)
    }
}
