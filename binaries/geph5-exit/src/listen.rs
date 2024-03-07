use anyhow::Context;
use ed25519_dalek::Signer;
use futures_util::{AsyncReadExt, TryFutureExt};
use geph5_broker_protocol::{BrokerClient, ExitDescriptor, Mac, Signed, DOMAIN_EXIT_DESCRIPTOR};
use geph5_misc_rpc::{
    exit::{ClientCryptHello, ClientExitCryptPipe, ClientHello, ExitHello, ExitHelloInner},
    read_prepend_length, write_prepend_length,
};
use picomux::PicoMux;
use sillad::{listener::Listener, tcp::TcpListener, EitherPipe, Pipe};
use smol::future::FutureExt as _;
use std::{
    net::IpAddr,
    str::FromStr,
    time::{Duration, SystemTime},
};
use stdcode::StdcodeSerializeExt;
use tap::Tap;
use x25519_dalek::{EphemeralSecret, PublicKey};

use crate::{broker::BrokerRpcTransport, proxy::proxy_stream, CONFIG_FILE, SIGNING_SECRET};

pub async fn listen_main() -> anyhow::Result<()> {
    let c2e = c2e_loop();
    let broker = broker_loop();
    c2e.race(broker).await
}

#[tracing::instrument]
async fn broker_loop() -> anyhow::Result<()> {
    match &CONFIG_FILE.wait().broker {
        Some(broker) => {
            let my_ip = IpAddr::from_str(
                String::from_utf8_lossy(
                    &reqwest::get("https://checkip.amazonaws.com/")
                        .await?
                        .bytes()
                        .await?,
                )
                .trim(),
            )?;
            tracing::info!(
                my_ip = display(my_ip),
                my_pubkey = display(hex::encode(SIGNING_SECRET.as_bytes())),
                "starting communication with broker"
            );
            let transport = BrokerRpcTransport::new(&broker.url);
            let client = BrokerClient(transport);
            loop {
                let descriptor = ExitDescriptor {
                    c2e_listen: CONFIG_FILE
                        .wait()
                        .c2e_listen
                        .tap_mut(|addr| addr.set_ip(my_ip)),
                    b2e_listen: CONFIG_FILE
                        .wait()
                        .b2e_listen
                        .tap_mut(|addr| addr.set_ip(my_ip)),
                    country: CONFIG_FILE.wait().country,
                    city: CONFIG_FILE.wait().city.clone(),
                    load: 0.0,
                    expiry: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        + 600,
                };
                let to_upload = Mac::new(
                    Signed::new(descriptor, DOMAIN_EXIT_DESCRIPTOR, &SIGNING_SECRET),
                    blake3::hash(broker.auth_token.as_bytes()).as_bytes(),
                );
                client
                    .put_exit(to_upload)
                    .await?
                    .map_err(|e| anyhow::anyhow!(e.0))?;

                smol::Timer::after(Duration::from_secs(60)).await;
            }
        }
        None => {
            tracing::info!("not starting broker loop since there's no binder URL");
            smol::future::pending().await
        }
    }
}

async fn c2e_loop() -> anyhow::Result<()> {
    let mut listener = TcpListener::bind(CONFIG_FILE.wait().c2e_listen).await?;
    loop {
        let c2e_raw = listener.accept().await?;
        smolscale::spawn(
            handle_client(c2e_raw).map_err(|e| tracing::debug!("client died suddenly with {e}")),
        )
        .detach()
    }
}

async fn handle_client(mut client: impl Pipe) -> anyhow::Result<()> {
    // execute the authentication
    let client_hello: ClientHello = stdcode::deserialize(&read_prepend_length(&mut client).await?)?;

    let keys: Option<([u8; 32], [u8; 32])>;
    let exit_hello_inner: ExitHelloInner = match client_hello.crypt_hello {
        ClientCryptHello::SharedSecretChallenge(key) => {
            let real_ss = client.shared_secret().context("no shared secret")?;
            let mac = blake3::keyed_hash(&key, real_ss);
            keys = None;
            ExitHelloInner::SharedSecretResponse(mac)
        }
        ClientCryptHello::X25519(their_epk) => {
            let my_esk = EphemeralSecret::random_from_rng(rand::thread_rng());
            let my_epk = PublicKey::from(&my_esk);
            let shared_secret = my_esk.diffie_hellman(&their_epk);
            let read_key = blake3::derive_key("c2e", shared_secret.as_bytes());
            let write_key = blake3::derive_key("e2c", shared_secret.as_bytes());
            keys = Some((read_key, write_key));
            ExitHelloInner::X25519(my_epk)
        }
    };
    let exit_hello = ExitHello {
        inner: exit_hello_inner.clone(),
        signature: SIGNING_SECRET.sign(&(client_hello, exit_hello_inner).stdcode()),
    };
    write_prepend_length(&exit_hello.stdcode(), &mut client).await?;

    let client = if let Some((read_key, write_key)) = keys {
        EitherPipe::Left(ClientExitCryptPipe::new(client, read_key, write_key))
    } else {
        EitherPipe::Right(client)
    };

    let (client_read, client_write) = client.split();
    let mut mux = PicoMux::new(client_read, client_write);
    loop {
        let stream = mux.accept().await?;
        smolscale::spawn(proxy_stream(stream).map_err(|e| tracing::debug!("stream died with {e}")))
            .detach();
    }
}
