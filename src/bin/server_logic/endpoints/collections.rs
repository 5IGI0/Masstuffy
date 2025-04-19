/**
 *  This file is part of Masstuffy. Masstuffy is free software:
 *  you can redistribute it and/or modify it under the terms of 
 *  the GNU Affero General Public License as published by
 *  the Free Software Foundation, either version 3 of the License,
 *  or (at your option) any later version.
 * 
 *  Masstuffy is distributed in the hope that it will be useful,
 *  but WITHOUT ANY WARRANTY; without even the implied warranty of
 *  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
 * 
 *  See the GNU Affero General Public License for more details.
 *  You should have received a copy of the GNU Affero General Public License
 *  along with Masstuffy. If not, see <https://www.gnu.org/licenses/>. 
 * 
 *  Copyright (C) 2025 5IGI0 / Ethan L. C. Lorenzetti
**/

use tokio::io::BufReader;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use masstuffy::{database::structs::RECORD_FLAG_ACTIVE, filesystem::CollID, warc::read_record};
use serde::Deserialize;
use serde_json::json;
use tide::{Request, Response};
use masstuffy::filesystem::collections::CollectionInfo;
use crate::server_logic::AppState;

pub async fn list_collections(req: Request<AppState>) -> tide::Result {
    let fs = req.state().fs.read().await;

    let mut collection_infos: Vec<CollectionInfo> = Vec::new();
    let collections = fs.get_collection_list().await;

    for col in collections {
        if let Some(col) = fs.get_collection(CollID::Slug(col)).await {
            collection_infos.push(col.read().await.get_info().await);
        }
    }

    Ok(
        Response::builder(200)
        .body(json!(collection_infos))
        .build())
}

#[derive(Deserialize)]
struct CreateCollectionParams {
    slug: String,
    dict_id: u32,
    comp_algo: Option<String>,
}

pub async fn create_collection(mut req: Request<AppState>) -> tide::Result {
    let data: CreateCollectionParams = req.body_json().await?;

    let dictionary = if let Some(algo) = data.comp_algo {
        Some((algo, data.dict_id))
    } else {
        None
    };

    let result = req.state().fs.write().await.
    create_collection(data.slug, dictionary).await?;

    Ok(Response::builder(200)
        .body(json!(result)).build())
}

pub async fn push_records(mut req: Request<AppState>) -> tide::Result {
    let body = req.take_body();
    let mut buf = BufReader::new(body.compat());

    let coll = req.state().fs.read().await
        .get_collection(CollID::Uuid(req.param("collection_uuid").unwrap().to_string())).await;

    if let None = coll {
        return Ok(Response::builder(404).body("collection not found").build());
    }
    let coll = coll.unwrap();
    let coll_uuid = coll.read().await.get_uuid().await;

    let (dict_id, dict_algo) = coll.read().await.get_dict().await;
    let dict_id = if let Some(dict_id) = dict_id {
        Some(dict_id as i64)
    } else {
        None
    };

    while let Some(record) = read_record(&mut buf).await? {
        let cdx = coll.read().await.add_warc(&record).await?;
        req.state().db.read().await.
        insert_record(&coll_uuid, &cdx, RECORD_FLAG_ACTIVE, dict_id, dict_algo.as_deref()).await?;
    }

    Ok(Response::builder(200).body("success").build())
}