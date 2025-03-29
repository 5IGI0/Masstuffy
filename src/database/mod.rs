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
 
use sqlx::postgres::PgPool;
use structs::DBWarcRecord;
use log::info;
 
use crate::{constants::MASSTUFFY_DATE_FMT, warc::cdx::CDXRecord};
use chrono::NaiveDateTime;


pub mod structs;

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
        sqlx::migrate!()
            .run(&self.db)
            .await.expect("unable to init db");

        self.is_setup = true;
    }

    // TODO: insert from iterator
    pub async fn insert_record(&self, coll: &str, record: &CDXRecord) -> anyhow::Result<()> {
        sqlx::query(r#"
        INSERT INTO masstuffy_records(
            flags, date, identifier,
            collection, filename, "offset", "type",
            uri)
        VALUES(
            0, to_timestamp($1, 'YYYYMMDDHH24MISS'), $2,
            $3, $4, $5, $6, $7)"#)
            .bind(record.get_date())
            .bind(record.get_record_id())
            .bind(coll)
            .bind(record.get_file_name().unwrap())
            .bind(record.get_file_offset().unwrap())
            .bind(record.get_record_type())
            .bind(record.get_url())
            .execute(&self.db).await?;
        Ok(())
    }

    pub async fn get_record_from_id(&self, id: String) -> anyhow::Result<DBWarcRecord> {
        let record: DBWarcRecord = sqlx::query_as!(DBWarcRecord,
            "SELECT * FROM masstuffy_records WHERE identifier=$1 LIMIT 1", id).fetch_one(&self.db).await?.into();
        Ok(record)
    }

    pub async fn get_record_from_uri(&self, date: &String, uri: &String) -> anyhow::Result<DBWarcRecord> {
        // TOOD: better way than comparing epoches?
        let record: DBWarcRecord = sqlx::query_as!(DBWarcRecord,
            r#"SELECT * FROM masstuffy_records
            WHERE
                "type" != 'request' AND
                uri=$1
            ORDER BY ABS(DATE_PART('epoch', date) - DATE_PART('epoch', $2::timestamp)) ASC
            LIMIT 1
            "#, uri, NaiveDateTime::parse_from_str(date, MASSTUFFY_DATE_FMT)?).fetch_one(&self.db).await?.into();
        Ok(record)
    }

    pub async fn get_samples(&self, collection: &str, limit: i64) -> anyhow::Result<Vec<DBWarcRecord>> {
        Ok(sqlx::query_as!(
            DBWarcRecord,
            r#"
            SELECT * FROM masstuffy_records
            WHERE collection=$1
            ORDER BY hashint8(id)
            LIMIT $2"#, collection, limit).
            fetch_all(&self.db).await?) // TODO: make it random?
    }
}