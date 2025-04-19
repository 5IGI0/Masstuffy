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

use chrono::NaiveDateTime;

#[derive(sqlx::FromRow)]
pub struct DBWarcRecord {
    pub id: i64,
    pub flags: i32,
    pub date: NaiveDateTime,
    pub identifier: String,
    pub collection: String,
    pub filename: String,
    pub offset: i64,
    pub r#type: String,
    pub uri: Option<String>,
    pub dict_type: Option<String>,
    pub dict_id: Option<i64>
}

pub const RECORD_FLAG_ACTIVE: i32 = 1<<0;