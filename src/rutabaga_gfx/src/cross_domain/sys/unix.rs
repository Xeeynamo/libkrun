// Copyright 2021 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::os::unix::prelude::AsFd;

use nix::cmsg_space;
use nix::sys::epoll::EpollCreateFlags;
use nix::sys::epoll::EpollFlags;
use nix::sys::eventfd::eventfd;
use nix::sys::eventfd::EfdFlags;
use nix::sys::socket::connect;
use nix::sys::socket::recvmsg;
use nix::sys::socket::sendmsg;
use nix::sys::socket::socket;
use nix::sys::socket::AddressFamily;
use nix::sys::socket::ControlMessage;
use nix::sys::socket::ControlMessageOwned;
use nix::sys::socket::MsgFlags;
use nix::sys::socket::SockFlag;
use nix::sys::socket::SockType;
use nix::sys::socket::UnixAddr;
use nix::unistd::pipe;
use nix::unistd::read;
use nix::unistd::write;

use super::super::add_item;
use super::super::cross_domain_protocol::CROSS_DOMAIN_ID_TYPE_READ_PIPE;
use super::super::cross_domain_protocol::CROSS_DOMAIN_ID_TYPE_VIRTGPU_BLOB;
use super::super::CrossDomainContext;
use super::super::CrossDomainItem;
use super::super::CrossDomainJob;
use super::super::CrossDomainState;
use super::epoll_internal::Epoll;
use super::epoll_internal::EpollEvent;
use crate::cross_domain::cross_domain_protocol::{
    CrossDomainInitV1, CrossDomainSendReceiveBase, CROSS_DOMAIN_ID_TYPE_SHM,
};
use crate::cross_domain::CrossDomainEvent;
use crate::cross_domain::CrossDomainToken;
use crate::cross_domain::WAIT_CONTEXT_MAX;
use crate::rutabaga_os::AsRawDescriptor;
use crate::rutabaga_os::FromRawDescriptor;
use crate::rutabaga_os::RawDescriptor;
use crate::RutabagaError;
use crate::RutabagaResult;

pub type SystemStream = File;

impl CrossDomainState {
    fn send_msg(&self, opaque_data: &[u8], descriptors: &[RawDescriptor]) -> RutabagaResult<usize> {
        let cmsg = ControlMessage::ScmRights(descriptors);
        if let Some(connection) = &self.connection {
            let bytes_sent = sendmsg::<()>(
                connection.as_raw_descriptor(),
                &[IoSlice::new(opaque_data)],
                &[cmsg],
                MsgFlags::empty(),
                None,
            )?;

            return Ok(bytes_sent);
        }

        Err(RutabagaError::InvalidCrossDomainChannel)
    }

    pub(crate) fn receive_msg<const MAX_IDENTIFIERS: usize>(
        &self,
        opaque_data: &mut [u8],
    ) -> RutabagaResult<(usize, Vec<File>)> {
        // If any errors happen, the socket will get dropped, preventing more reading.
        let mut iovecs = [IoSliceMut::new(opaque_data)];
        let mut cmsgspace = cmsg_space!([RawDescriptor; MAX_IDENTIFIERS]);
        let flags = MsgFlags::empty();

        if let Some(connection) = &self.connection {
            let r = recvmsg::<()>(
                connection.as_raw_descriptor(),
                &mut iovecs,
                Some(&mut cmsgspace),
                flags,
            )?;
            let len = r.bytes;

            let files = match r.cmsgs().next() {
                Some(ControlMessageOwned::ScmRights(fds)) => {
                    fds.into_iter()
                        .map(|fd| {
                            // Safe since the descriptors from recv_with_fds(..) are owned by us and valid.
                            unsafe { File::from_raw_descriptor(fd) }
                        })
                        .collect()
                }
                Some(_) => return Err(RutabagaError::Unsupported),
                None => Vec::new(),
            };

            Ok((len, files))
        } else {
            Err(RutabagaError::InvalidCrossDomainChannel)
        }
    }
}

impl CrossDomainContext {
    pub(crate) fn get_connection(
        &mut self,
        cmd_init: &CrossDomainInitV1,
    ) -> RutabagaResult<Option<SystemStream>> {
        let channels = self
            .channels
            .take()
            .ok_or(RutabagaError::InvalidCrossDomainChannel)?;
        let base_channel = &channels
            .iter()
            .find(|channel| channel.channel_type == cmd_init.channel_type)
            .ok_or(RutabagaError::InvalidCrossDomainChannel)?
            .base_channel;

        let socket_fd = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;

        let unix_addr = UnixAddr::new(base_channel)?;
        connect(socket_fd, &unix_addr)?;
        let stream = unsafe { File::from_raw_fd(socket_fd) };
        Ok(Some(stream))
    }

    pub(crate) fn send<T: CrossDomainSendReceiveBase, const MAX_IDENTIFIERS: usize>(
        &self,
        cmd_send: &mut T,
        opaque_data: &[u8],
    ) -> RutabagaResult<()> {
        let mut descriptors = [0; MAX_IDENTIFIERS];

        let mut write_pipe_opt: Option<File> = None;
        let mut read_pipe_id_opt: Option<u32> = None;

        let num_identifiers = (*cmd_send.num_identifiers_mut()).try_into()?;

        if num_identifiers > MAX_IDENTIFIERS {
            return Err(RutabagaError::SpecViolation(
                "max cross domain identifiers exceeded",
            ));
        }

        let iter = cmd_send
            .iter_over_identifiers()
            .zip(descriptors.iter_mut())
            .take(num_identifiers);

        for ((identifier, identifier_type, _), descriptor) in iter {
            if *identifier_type == CROSS_DOMAIN_ID_TYPE_VIRTGPU_BLOB {
                let context_resources = self.context_resources.lock().unwrap();

                let context_resource = context_resources
                    .get(identifier)
                    .ok_or(RutabagaError::InvalidResourceId)?;

                if let Some(ref handle) = context_resource.handle {
                    *descriptor = handle.os_handle.as_raw_descriptor();
                } else {
                    return Err(RutabagaError::InvalidRutabagaHandle);
                }
            } else if *identifier_type == CROSS_DOMAIN_ID_TYPE_READ_PIPE {
                // In practice, just 1 pipe pair per send is observed.  If we encounter
                // more, this can be changed later.
                if write_pipe_opt.is_some() {
                    return Err(RutabagaError::SpecViolation("expected just one pipe pair"));
                }

                let (raw_read_pipe, raw_write_pipe) = pipe()?;
                let read_pipe = unsafe { File::from_raw_descriptor(raw_read_pipe) };
                let write_pipe = unsafe { File::from_raw_descriptor(raw_write_pipe) };

                *descriptor = write_pipe.as_raw_descriptor();
                let read_pipe_id: u32 = add_item(
                    &self.item_state,
                    CrossDomainItem::WaylandReadPipe(read_pipe),
                );

                // For Wayland read pipes, the guest guesses which identifier the host will use to
                // avoid waiting for the host to generate one.  Validate guess here.  This works
                // because of the way Sommelier copy + paste works.  If the Sommelier sequence of events
                // changes, it's always possible to wait for the host response.
                if read_pipe_id != *identifier {
                    return Err(RutabagaError::InvalidCrossDomainItemId);
                }

                // The write pipe needs to be dropped after the send_msg(..) call is complete, so the read pipe
                // can receive subsequent hang-up events.
                write_pipe_opt = Some(write_pipe);
                read_pipe_id_opt = Some(read_pipe_id);
            } else if *identifier_type == CROSS_DOMAIN_ID_TYPE_SHM {
                if let Some(ftx) = self.futexes.lock().unwrap().get(identifier) {
                    *descriptor = ftx.handle.as_raw_descriptor();
                } else {
                    return Err(RutabagaError::InvalidCrossDomainItemId);
                }
            } else {
                // Don't know how to handle anything else yet.
                return Err(RutabagaError::InvalidCrossDomainItemType);
            }
        }

        if let (Some(state), Some(resample_evt)) = (&self.state, &self.resample_evt) {
            state.send_msg(opaque_data, &descriptors[..num_identifiers])?;

            if let Some(read_pipe_id) = read_pipe_id_opt {
                state.add_job(CrossDomainJob::AddReadPipe(read_pipe_id));
                channel_signal(resample_evt)?;
            }
        } else {
            return Err(RutabagaError::InvalidCrossDomainState);
        }

        Ok(())
    }
}

pub type Sender = File;
pub type Receiver = File;

pub fn channel_signal(sender: &Sender) -> RutabagaResult<()> {
    write(sender.as_raw_fd(), &1u64.to_ne_bytes())?;
    Ok(())
}

pub fn channel_wait(receiver: &Receiver) -> RutabagaResult<()> {
    read(receiver.as_raw_fd(), &mut 1u64.to_ne_bytes())?;
    Ok(())
}

pub fn read_volatile(file: &File, opaque_data: &mut [u8]) -> RutabagaResult<usize> {
    let bytes_read = read(file.as_raw_fd(), opaque_data)?;
    Ok(bytes_read)
}

pub fn write_volatile(file: &File, opaque_data: &[u8]) -> RutabagaResult<()> {
    write(file.as_raw_fd(), opaque_data)?;
    Ok(())
}

pub fn channel() -> RutabagaResult<(Sender, Receiver)> {
    let sender = unsafe { File::from_raw_fd(eventfd(0, EfdFlags::empty())?) };
    let receiver = sender.try_clone()?;
    Ok((sender, receiver))
}

pub struct WaitContext {
    epoll_ctx: Epoll,
    data: u64,
    vec: Vec<(u64, CrossDomainToken)>,
}

impl WaitContext {
    pub fn new() -> RutabagaResult<WaitContext> {
        let epoll = Epoll::new(EpollCreateFlags::empty())?;
        Ok(WaitContext {
            epoll_ctx: epoll,
            data: 0,
            vec: Default::default(),
        })
    }

    pub fn add<Waitable: AsFd>(
        &mut self,
        token: CrossDomainToken,
        waitable: Waitable,
    ) -> RutabagaResult<()> {
        self.data += 1;
        self.epoll_ctx
            .add(waitable, EpollEvent::new(EpollFlags::EPOLLIN, self.data))?;
        self.vec.push((self.data, token));
        Ok(())
    }

    fn calculate_token(&self, data: u64) -> RutabagaResult<CrossDomainToken> {
        if let Some(item) = self.vec.iter().find(|item| item.0 == data) {
            return Ok(item.1);
        }

        Err(RutabagaError::SpecViolation("unable to find token"))
    }

    pub fn wait(&mut self) -> RutabagaResult<Vec<CrossDomainEvent>> {
        let mut events = [EpollEvent::empty(); WAIT_CONTEXT_MAX];
        let count = loop {
            break match self.epoll_ctx.wait(&mut events, isize::MAX) {
                Err(nix::errno::Errno::EINTR) => continue,
                a => a,
            };
        }?;
        let events = events[0..count]
            .iter()
            .map(|e| CrossDomainEvent {
                token: self.calculate_token(e.data()).unwrap(),
                readable: e.events() & EpollFlags::EPOLLIN == EpollFlags::EPOLLIN,
                hung_up: e.events() & EpollFlags::EPOLLHUP == EpollFlags::EPOLLHUP
                    || e.events() & EpollFlags::EPOLLRDHUP == EpollFlags::EPOLLRDHUP,
            })
            .collect();

        Ok(events)
    }

    pub fn delete<Waitable: AsFd>(
        &mut self,
        token: CrossDomainToken,
        waitable: Waitable,
    ) -> RutabagaResult<()> {
        self.epoll_ctx.delete(waitable)?;
        self.vec.retain(|item| item.1 != token);
        Ok(())
    }
}
