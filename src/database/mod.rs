use log::info;
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

use sqlx::{postgres::PgPool, Executor};

use crate::warc::cdx::CDXRecord;

pub struct DBManager {
    is_setup: bool,
    db: PgPool
}

impl DBManager {
    pub fn new(connect: &str) -> Self {
        DBManager {
            is_setup: false,
            db: PgPool::connect_lazy(connect).expect("cannot connect to database"),
        }
    }

    pub async fn setup_db(&mut self) {
        if self.is_setup {
            return
        }

        info!("setting up database...");

        // TODO: [improvement] use numeric value for collections (or maybe enum?) and filename
        // TODO: massaged url
        self.db.execute( r#"
        CREATE TABLE IF NOT EXISTS masstuffy_records (
            id         bigserial,
            flags      int4,
            date       timestamp,
            identifier text,
            collection text,
            filename   text,
            "offset"   bigint,
            "type"     text
        );"#).await.expect("unable to create record table");

        self.db.execute( r#"
        CREATE INDEX IF NOT EXISTS masstuffy_record_id_idx
        ON masstuffy_records USING hash (identifier);"#).await.expect("unable to create record index (masstuffy_record_id_idx)");
        self.db.execute( r#"
        CREATE UNIQUE INDEX IF NOT EXISTS masstuffy_record_id_unq
        ON masstuffy_records(identifier);"#).await.expect("unable to create record index (masstuffy_record_id_unq)");

        self.is_setup = true;
    }

    // TODO: insert from iterator
    pub async fn insert_record(&mut self, coll: &str, record: &CDXRecord) -> anyhow::Result<()> {
        sqlx::query(r#"
        INSERT INTO masstuffy_records(
            flags, date, identifier,
            collection, filename, "offset", "type")
        VALUES(
            0, to_timestamp($1, 'YYYYMMDDHH24MISS'), $2,
            $3, $4, $5, $6)"#)
            .bind(record.get_date())
            .bind(record.get_record_id())
            .bind(coll)
            .bind(record.get_file_name().unwrap())
            .bind(record.get_file_offset().unwrap())
            .bind(record.get_record_type())
            .execute(&self.db).await?;
        Ok(())
    }
}