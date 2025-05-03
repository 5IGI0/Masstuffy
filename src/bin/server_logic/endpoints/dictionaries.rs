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

use tide::{Request, Response};
use crate::server_logic::AppState;

pub async fn get_dictionary(req: Request<AppState>) -> tide::Result {
    let dict = req.state().fs.read().await
        .get_zstd_dict(req.param("dict_id")?.parse()?)
        .await;

    if let Some(dict) = dict {
        Ok(Response::builder(200)
            .body(dict.as_slice()).build())
    } else {
        Ok(Response::builder(404)
            .body("404 Not Found\n").build())
    }
}