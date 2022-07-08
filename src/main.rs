extern crate core;

use std::error::Error;
use std::{mem, thread};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use ricq::{Client, LoginResponse, QRCodeConfirmed, QRCodeImageFetch, QRCodeState, version};
use ricq::client::Token;
use ricq::device::Device;
use ricq::ext::common::after_login;
use ricq::handler::DefaultHandler;
use tokio::{fs, io, runtime};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpSocket;
use tokio::task::yield_now;
use tracing::{debug, error, info, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static HELP_INFO: &str = "\
RQ Login Helper
help -> Show this info
login <qq> <password> -> Login with password
qrlogin <qq> -> Login with qrcode

exit | quit -> Close this program
-------------------------------------------------
After login, the device info will automatically generate and write
to 'device.json', and the login token will write to 'token.json'
";

static WELCOME_INFO: &str = "\
--------------------------
Welcome to RQ Login Helper
--------------------------
This program can help you to login qq
and generate the device info.

Author: LaoLittle (https://github.com/LaoLittle)
";

macro_rules! unwrap_option_or_help {
    ($($x:tt)+) => {
        match ($($x)+) {
            Some(s) => s,
            None => {
                println!("{}", HELP_INFO);
                continue;
            },
        }
    };
}

macro_rules! unwrap_result_or_err {
    ($($x:tt)+) => {
        match ($($x)+) {
            Ok(s) => s,
            Err(e) => {
                error!("{:?}", e);
                continue;
            },
        }
    };
}

type MainResult = Result<(), Box<dyn Error>>;

fn main() -> MainResult {
    let rt = runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;

    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::filter_fn(|m| {
            match m.level() {
                &Level::TRACE => false,
                _ => true
            }
        }))
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .init();

    println!("{}", WELCOME_INFO);

    rt.block_on(main0())
}

async fn main0() -> MainResult {
    let mut stdout = tokio::io::stdout();
    let stdin = tokio::io::stdin();
    let mut stdin = BufReader::new(stdin);

    let mut buf = String::new();

    'main:
    loop {
        //print!(">>");
        stdout.write_all(">>".as_bytes()).await?;
        stdout.flush().await?;

        stdin.read_line(&mut buf).await?;

        let s = mem::take(&mut buf);
        let s = s.trim_end();

        let spl: Vec<&str> = s.split(' ').collect();
        let first = *spl.first().expect("Cannot be None");

        let mut p = PathBuf::new();
        match first {
            "" => println!(),
            "help" => println!("{}", HELP_INFO),
            "exit" | "quit" => break,
            "login" => {
                let account = unwrap_option_or_help!(spl.get(1));
                p.push(account);
                if !p.is_dir() { fs::create_dir(&p).await?; }

                let account = unwrap_result_or_err!(i64::from_str(account));

                let password = unwrap_option_or_help!(spl.get(2));

                let client = get_client(&p).await?;
                let mut resp = client.password_login(account, password).await?;

                println!("{:?}", resp);
            }
            "qrlogin" => {
                let account = unwrap_option_or_help!(spl.get(1));
                unwrap_result_or_err!(i64::from_str(account));
                p.push(account);
                if !p.is_dir() { fs::create_dir(&p).await?; }

                let client = get_client(&p).await?;

                let mut state = client.fetch_qrcode().await?;

                let mut img_file = fs::File::create("qr.png").await?;
                let mut signature = None::<Bytes>;
                loop {
                    match state {
                        QRCodeState::ImageFetch(
                            QRCodeImageFetch {
                                ref image_data,
                                ref sig
                            }) => {
                            img_file.write_all(image_data).await?;
                            signature = Some(sig.clone());
                            info!("已获取二维码，位于 ./qr.png");
                        }
                        QRCodeState::WaitingForScan => {
                            info!("等待扫码");
                        }
                        QRCodeState::WaitingForConfirm => {
                            info!("已扫码，等待确认");
                        }
                        QRCodeState::Confirmed(
                            QRCodeConfirmed {
                                ref tmp_pwd,
                                ref tmp_no_pic_sig,
                                ref tgt_qr,
                                ..
                            }) => {
                            let mut login_resp = client.qrcode_login(
                                tmp_pwd,
                                tmp_no_pic_sig,
                                tgt_qr,
                            ).await?;

                            if let LoginResponse::DeviceLockLogin(..) = login_resp {
                                login_resp = client.device_lock_login().await?;
                            } else {
                                panic!("Not device lock login: {:?}", login_resp);
                            }

                            info!("登陆成功");
                            debug!("{:?}", login_resp);

                            break;
                        }
                        QRCodeState::Canceled => {
                            error!("已取消扫码");
                            continue 'main;
                        }
                        QRCodeState::Timeout => {
                            state = client.fetch_qrcode().await?;
                            if let QRCodeState::ImageFetch(ref fe) = state {
                                img_file.write_all(&fe.image_data).await?;
                                signature = Some(fe.sig.clone());
                            }

                            error!("超时，重新生成二维码");
                        }
                    }

                    tokio::time::sleep(Duration::from_secs(5)).await;

                    if let Some(ref sig) = signature {
                        state = client.query_qrcode_result(sig).await?;
                    }
                }

                after_login(&client).await;

                let token = client.gen_token().await;
                write_token_file(&token, &p).await?;
            }
            _ => println!("Unknown command '{}', use 'help' to show the help info", first)
        }
    }

    Ok(())
}

async fn device_or_default<P: AsRef<Path>>(dir: P) -> Device {
    let mut buf = PathBuf::new();
    buf.push(dir);
    buf.push("device.json");

    if !buf.is_file() {
        let d = Device::random();
        let s = serde_json::to_string_pretty(&d).expect("Serialization fault");
        let mut f = fs::File::create(&buf).await.expect("Cannot create file");
        f.write_all(s.as_bytes()).await.ok();

        return d;
    }

    let mut f = fs::File::open(&buf).await.expect("Cannot open file");
    let mut s = String::new();
    f.read_to_string(&mut s).await.expect("Cannot read file");

    serde_json::from_str(&s).unwrap_or_else(|_| Device::random())
}

async fn get_client<P: AsRef<Path>>(dir: P) -> io::Result<Arc<Client>> {
    let device = device_or_default(dir).await;

    let client = Client::new(
        device,
        version::ANDROID_WATCH,
        DefaultHandler,
    );
    let client = Arc::new(client);

    //let addr = SocketAddr::new(Ipv4Addr::new(113, 96, 18, 253).into(), 80);
    let socket = TcpSocket::new_v4()?;
    let stream = socket.connect(client.get_address()).await?;

    let client0 = client.clone();
    tokio::spawn(async move { client0.start(stream).await; });
    yield_now().await;

    Ok(client)
}

async fn write_token_file<P: AsRef<Path>>(token: &Token, dir: P) -> io::Result<()> {
    let mut buf = PathBuf::new();
    buf.push(dir);
    buf.push("token.json");

    let mut f = fs::File::create(&buf).await?;
    let s = serde_json::to_string_pretty(token).expect("Serialization fault");

    f.write_all(s.as_bytes()).await?;

    Ok(())
}