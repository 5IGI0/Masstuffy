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
 *  See the GNU General Public License for more details.
 *  You should have received a copy of the GNU General Public License
 *  along with Masstuffy. If not, see <https://www.gnu.org/licenses/>. 
 * 
 *  Copyright (C) 2025 5IGI0 / Ethan L. C. Lorenzetti
**/

use masstuffy::filesystem::CollID;
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