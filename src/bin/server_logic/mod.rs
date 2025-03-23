use std::sync::Arc;

use masstuffy::{database::DBManager, filesystem::{FileSystem, self}};
use serde::{Deserialize, Serialize};
use tide::{Body, Request, Response};
use tokio::sync::RwLock;

#[derive(Clone)]
struct AppState {
    fs: Arc<RwLock<FileSystem>>,
    db: Arc<RwLock<DBManager>>
}

async fn get_by_id(req: Request<AppState>) -> tide::Result {
    let cdx_rec = req.state().db.read().await
            .get_record_from_id(req.param("id").unwrap().to_string()).await?;
    
    let rec = req.state().fs.read().await.get_record(
        &cdx_rec.collection, &cdx_rec.filename, cdx_rec.offset)?.unwrap();

    Ok(Response::builder(200)
    .body(rec.serialize())
    .build())
}

#[derive(Deserialize, Serialize)]
struct ServerStatus {
    repository: String,
    version: String
}

async fn server_status_handler(_: Request<AppState>) -> tide::Result {
    Ok(Response::from(Body::from_json(&ServerStatus{
        repository: env!("CARGO_PKG_HOMEPAGE").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string()
    }).expect("error serializing")))
}

pub fn main() {
    env_logger::init();

    let fs = filesystem::init().expect("unable to init filesystem"); //Arc<RwLock<FileSystem>> = ;
    
    let listen_addr = fs.get_listen_addr();
    let database_conn = fs.get_database_conn_string();

    let state = AppState{
        fs: Arc::new(RwLock::new(fs)),
        db: Arc::new(RwLock::new(DBManager::new(&database_conn)))
        };

    
    let mut app = tide::with_state(state);
    app.at("/").get(server_status_handler);
    app.at("/id/:flags/:id").get(get_by_id);
    let rt = tokio::runtime::Runtime::new().expect("unable to start tokio runtime");
    rt.block_on(app.listen(listen_addr)).expect("failed block_on");
}