//! A Modbus TCP server on top of a [`Bank`], answering function codes 03, 04, 06 and 16.

use std::future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_modbus::server::Service;
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
use tokio_modbus::{ExceptionCode, Request, Response, SlaveRequest};

use super::bank::Bank;
use super::map::Table;

/// A client's write, for the device's log and its behaviour.
#[derive(Debug, Clone)]
pub struct Write {
    pub unit: u8,
    pub address: u16,
    pub words: Vec<u16>,
    pub registers: Vec<String>,
}

struct Registers {
    bank: Arc<Mutex<Bank>>,
    writes: mpsc::UnboundedSender<Write>,
}

impl Service for Registers {
    type Request = SlaveRequest<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Response, ExceptionCode>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let unit = req.slave;
        let Ok(mut bank) = self.bank.lock() else {
            return future::ready(Err(ExceptionCode::ServerDeviceFailure));
        };
        let result = match req.request {
            Request::ReadInputRegisters(address, quantity) => bank
                .read(unit, Table::Input, address, quantity)
                .map(Response::ReadInputRegisters),
            Request::ReadHoldingRegisters(address, quantity) => bank
                .read(unit, Table::Holding, address, quantity)
                .map(Response::ReadHoldingRegisters),
            Request::WriteSingleRegister(address, word) => {
                bank.write(unit, address, &[word]).map(|registers| {
                    let _ = self.writes.send(Write { unit, address, words: vec![word], registers });
                    Response::WriteSingleRegister(address, word)
                })
            }
            Request::WriteMultipleRegisters(address, words) => {
                let words = words.into_owned();
                let quantity = words.len() as u16;
                bank.write(unit, address, &words).map(|registers| {
                    let _ = self.writes.send(Write { unit, address, words, registers });
                    Response::WriteMultipleRegisters(address, quantity)
                })
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(result)
    }
}

/// Binds and serves in the background; returns the bound address.
pub async fn serve(
    address: SocketAddr,
    bank: Arc<Mutex<Bank>>,
    writes: mpsc::UnboundedSender<Write>,
) -> Result<SocketAddr, String> {
    let listener = TcpListener::bind(address)
        .await
        .map_err(|e| format!("cannot listen on {address}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let server = Server::new(listener);
    tokio::spawn(async move {
        let new_service = move |_peer: SocketAddr| {
            Ok(Some(Registers {
                bank: bank.clone(),
                writes: writes.clone(),
            }))
        };
        let on_connected = |stream, peer| {
            let new_service = new_service.clone();
            async move { accept_tcp_connection(stream, peer, new_service) }
        };
        let _ = server
            .serve(&on_connected, |error| tracing::debug!("modbus client: {error}"))
            .await;
    });
    Ok(bound)
}
