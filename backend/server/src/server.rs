use diesel::pg::PgConnection;
use dotenv::dotenv;
use futures::future;
use futures_cpupool::CpuPool;
use futures::future::{Either, FutureResult};
use futures::{Future, Stream};
use hyper;
use hyper::{Client, Chunk, Method, StatusCode};
use hyper::client::HttpConnector;
use hyper::server::{Http, Request, Response, Service};
use hyper::Headers;
use hyper::header::{AcceptLanguage, ContentLength, Authorization, Basic};
use hyper_tls::HttpsConnector;
use models::{NewAd, Ad};
use r2d2_diesel::ConnectionManager;
use r2d2::{Pool, Config};
use start_logging;
use serde_json;
use std::fs::File;
use std::io::Read;
use std::env;
use tokio_core::net::TcpListener;
use tokio_core::reactor::{Core, Handle};

use super::InsertError;

pub struct AdServer {
    db_pool: Pool<ConnectionManager<PgConnection>>,
    pool: CpuPool,
    handle: Handle,
    client: Client<HttpsConnector<HttpConnector>>,
}

#[derive(Deserialize, Debug)]
pub struct AdPost {
    pub id: String,
    pub html: String,
    pub political: Option<bool>,
    pub targeting: Option<String>,
}

impl Service for AdServer {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = Either<
        FutureResult<Self::Response, Self::Error>,
        Box<Future<Item = Self::Response, Error = Self::Error>>,
    >;
    // This is not at all RESTful, but I'd rather not deal with regexes. Maybe soon
    // someone will make a hyper router that's nice.
    fn call(&self, req: Request) -> Self::Future {
        match (req.method(), req.path()) {
            (&Method::Get, "/facebook-ads/admin") => Either::B(self.auth(
                req,
                || self.get_file("public/admin.html"),
            )),
            (&Method::Get, "/facebook-ads/admin.js") => Either::B(self.auth(
                req,
                || self.get_file("public/admin.js"),
            )),
            //(&Method::Post, "/facebook-ads/admin/ads") => Either::B(self.mark_ads()),
            (&Method::Get, "/facebook-ads/ads") => Either::B(self.get_ads(req)),
            (&Method::Post, "/facebook-ads/ads") => Either::B(self.process_ads(req)),
            (&Method::Get, "/facebook-ads/heartbeat") => Either::A(
                future::ok(Response::new().with_status(
                    StatusCode::Ok,
                )),
            ),
            _ => {
                Either::A(future::ok(
                    Response::new().with_status(StatusCode::NotFound),
                ))
            }
        }
    }
}

// I'm not happy with the OK OK OKs here, but I can't quite find a Result
// method that works. I should ask on stack overflow or something.
type ResponseFuture = Box<Future<Item = Response, Error = hyper::Error>>;
impl AdServer {
    fn auth<F>(&self, req: Request, callback: F) -> ResponseFuture
    where
        F: Fn() -> ResponseFuture,
    {
        if let Some(auth) = req.headers().get::<Authorization<Basic>>() {
            if auth ==
                &Authorization(Basic {
                    username: "propublica".to_string(),
                    password: Some("I want to moderate the ads.".to_string()),
                })
            {
                return callback();
            }
        }
        Box::new(future::ok(
            (Response::new().with_status(StatusCode::Unauthorized)),
        ))
    }


    fn get_file(&self, path: &str) -> ResponseFuture {
        let pool = self.pool.clone();
        let path = path.to_string();
        let future = pool.spawn_fn(move || {
            if let Ok(mut file) = File::open(path) {
                let mut buf = String::new();
                if let Ok(size) = file.read_to_string(&mut buf) {
                    return Ok(
                        Response::new()
                            .with_header(ContentLength(size as u64))
                            .with_body(buf),
                    );
                }
            }
            Ok(Response::new().with_status(StatusCode::NotFound))
        });
        Box::new(future)
    }

    fn get_lang_from_headers(headers: &Headers) -> Option<String> {
        if let Some(langs) = headers.get::<AcceptLanguage>() {
            if langs.len() == 0 {
                return None;
            }
            let mut languages = langs.to_owned();
            languages.sort_by(|a, b| b.quality.cmp(&a.quality));
            let lang = languages.iter().find(|quality| {
                quality.item.language.is_some() && quality.item.region.is_some()
            });
            if let Some(l) = lang {
                Some(
                    l.clone().item.language.unwrap() + "-" + &l.clone().item.region.unwrap(),
                )
            } else {
                languages[0].clone().item.language
            }
        } else {
            None
        }
    }

    fn get_ads(&self, req: Request) -> ResponseFuture {
        let db_pool = self.db_pool.clone();
        let pool = self.pool.clone();
        let future = if let Some(lang) = AdServer::get_lang_from_headers(req.headers()) {
            pool.spawn_fn(move || {
                if let Ok(ads) = Ad::get_ads_by_lang(&lang, &db_pool) {
                    if let Ok(serialized) = serde_json::to_string(&ads) {
                        return Ok(
                            Response::new()
                                .with_header(ContentLength(serialized.len() as u64))
                                .with_body(serialized),
                        );
                    }
                }
                Ok(Response::new().with_status(StatusCode::BadRequest))
            })
        } else {
            pool.spawn_fn(|| Ok(Response::new().with_status(StatusCode::BadRequest)))
        };
        Box::new(future)
    }

    fn process_ads(&self, req: Request) -> ResponseFuture {
        let db_pool = self.db_pool.clone();
        let pool = self.pool.clone();
        let image_pool = self.pool.clone();
        let image_db = self.db_pool.clone();
        let handle = self.handle.clone();
        let client = self.client.clone();
        let maybe_lang = AdServer::get_lang_from_headers(req.headers());
        if !maybe_lang.is_some() {
            return Box::new(future::ok(
                (Response::new().with_status(StatusCode::BadRequest)),
            ));
        };
        let lang = maybe_lang.unwrap();

        let future = req.body()
            .concat2()
            .then(move |msg| {
                pool.spawn_fn(move || AdServer::save_ads(msg, &db_pool, lang))
            })
            .and_then(move |ads| {
                for ad in ads {
                    handle.spawn(ad.grab_and_store(
                        client.clone(),
                        &image_db,
                        image_pool.clone(),
                    ))
                }

                Ok(Response::new())
            })
            .then(|r| match r {
                Ok(r) => Ok(r),
                Err(e) => {
                    warn!("{:?}", e);
                    Ok(Response::new().with_status(StatusCode::BadRequest))
                }
            });
        Box::new(future)
    }

    fn save_ads(
        msg: Result<Chunk, hyper::Error>,
        db_pool: &Pool<ConnectionManager<PgConnection>>,
        lang: String,
    ) -> Result<Vec<Ad>, InsertError> {
        let bytes = msg.map_err(InsertError::Hyper)?;
        let string = String::from_utf8(bytes.to_vec()).map_err(
            InsertError::String,
        )?;

        let posts: Vec<AdPost> = serde_json::from_str(&string).map_err(InsertError::JSON)?;
        let ads = posts.iter().map(move |post| {
            let ad = NewAd::new(post, &lang)?.save(db_pool)?;
            Ok(ad)
        });

        ads.collect::<Result<Vec<Ad>, InsertError>>()
    }

    pub fn start() {
        dotenv().ok();
        start_logging();
        let addr = env::var("HOST").expect("HOST must be set").parse().expect(
            "Error parsing HOST",
        );
        let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let config = Config::default();
        let manager = ConnectionManager::<PgConnection>::new(database_url);
        let db_pool = Pool::new(config, manager).expect("Failed to create pool.");
        let pool = CpuPool::new_num_cpus();

        let mut core = Core::new().unwrap();
        let handle = core.handle();
        let listener = TcpListener::bind(&addr, &handle).expect("Couldn't start server.");
        let connector =
            HttpsConnector::new(4, &core.handle()).expect("Couldn't build HttpsConnector");
        let client = Client::configure().connector(connector).build(
            &core.handle(),
        );

        let server = listener.incoming().for_each(|(sock, addr)| {
            let s = AdServer {
                pool: pool.clone(),
                db_pool: db_pool.clone(),
                handle: handle.clone(),
                client: client.clone(),
            };
            Http::new().bind_connection(&handle, sock, addr, s);

            Ok(())
        });

        println!("Listening on http://{} with 1 thread.", addr);
        core.run(server).unwrap();
    }
}
