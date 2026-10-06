// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this file,
// You can obtain one at http://mozilla.org/MPL/2.0/.

//! Retrieval of crash annotations through the minidump-writer remote executor

use {
    crash_helper_common::crash_annotations::CrashAnnotation,
    libc::pid_t,
    mozannotation_server::{
        errors::AnnotationsRetrievalError, CAnnotation, ProcessAccess, ReadError,
    },
    process_backend::{
        remote::{backend::Backend, transport},
        Backend as _, FileReader as _, ProcessReader as _,
    },
    serde::{Deserialize, Serialize},
    std::{ffi::CString, io, mem::ManuallyDrop},
};

/// Lends a transport to a [`Backend`] without giving up ownership of it.
#[derive(Debug)]
struct BorrowedTransport<'a, T>(&'a mut T);

impl<T: transport::Backend> transport::Backend for BorrowedTransport<'_, T> {
    fn max_response_output_len(&self) -> usize {
        self.0.max_response_output_len()
    }

    fn send_request<'output, Req: Serialize, Resp: Deserialize<'output>>(
        &'output mut self,
        req: Req,
    ) -> Result<Resp, transport::BackendError> {
        self.0.send_request(req)
    }
}

struct RemoteAccess<'a, T: transport::Backend> {
    backend: &'a Backend<T>,
    pid: pid_t,
}

impl<T: transport::Backend> ProcessAccess for RemoteAccess<'_, T> {
    fn read_memory(&self, address: usize, buf: &mut [u8]) -> Result<(), ReadError> {
        let reader = self.backend.process_reader();
        let mut offset = 0;
        while offset < buf.len() {
            let read = reader
                .read_at(address + offset, &mut buf[offset..])
                .map_err(|e| ReadError::AccessError(Box::new(e)))?;
            if read == 0 {
                return Err(ReadError::AccessError("unexpected end of memory".into()));
            }
            offset += read;
        }
        Ok(())
    }

    fn read_maps(&self) -> Result<String, io::Error> {
        let path = CString::new(format!("/proc/{}/maps", self.pid)).map_err(io::Error::other)?;
        let mut file = self.backend.read_file(&path).map_err(io::Error::other)?;
        let mut contents = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let read = file.read(&mut buf).map_err(io::Error::other)?;
            if read == 0 {
                break;
            }
            contents.extend_from_slice(&buf[..read]);
        }
        String::from_utf8(contents).map_err(io::Error::other)
    }
}

/// Read the annotations of the crashed process `pid` through the remote executor at the other end
/// of `transport`.
///
/// The executor is left running so that `transport` can be used to write the minidump afterwards.
pub(crate) fn retrieve<T: transport::Backend>(
    transport: &mut T,
    pid: pid_t,
) -> Result<Vec<CAnnotation>, AnnotationsRetrievalError> {
    // Dropping a `Backend` tells the executor to quit.
    let backend = ManuallyDrop::new(Backend::new(BorrowedTransport(transport)));
    let access = RemoteAccess {
        backend: &backend,
        pid,
    };
    mozannotation_server::retrieve_annotations_with_access(
        pid,
        &access,
        CrashAnnotation::Count as usize,
    )
}
