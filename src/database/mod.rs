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
 
use anyhow::Ok;
use chrono::NaiveDateTime;
use sqlx::postgres::PgPool;
use structs::DBWarcRecord;
use log::info;

use crate::{constants::MASSTUFFY_DATE_FMT, database::structs::DBToken, permissions::TokenInfo, warc::{cdx::CDXRecord, massaged_url::{massage_url, massaged_url_pattern, Match}}};

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
        sqlx::migrate!()
            .run(&self.db)
            .await.expect("unable to init db");

        self.is_setup = true;
    }

    // TODO: insert from iterator
    pub async fn insert_record(&self, coll: &str, record: &CDXRecord, flags: i32, dict_id: Option<i64>, dict_type: Option<&str>) -> anyhow::Result<()> {
        let massaged_url = massage_url(record.get_url().as_deref().unwrap_or("")).unwrap_or("".to_string());

        sqlx::query!(r#"
        INSERT INTO masstuffy_records(
            flags, date, identifier,
            collection, filename, "offset", "type",
            uri, dict_id, dict_type, massaged_url,
            raw_size)
        VALUES(
            $1, to_timestamp($2, 'YYYYMMDDHH24MISS'), $3,
            $4, $5, $6, $7, $8, $9, $10, $11,
            $12)"#,
        flags, record.get_date(), record.get_record_id(),
        coll, record.get_file_name().unwrap(), record.get_file_offset().unwrap(),
        record.get_record_type(), record.get_url(), dict_id, dict_type,
        massaged_url, record.get_raw_size().unwrap() as i32)
            .execute(&self.db).await?;
        Ok(())
    }

    pub async fn get_record_from_id(&self, id: String) -> anyhow::Result<DBWarcRecord> {
        let record: DBWarcRecord = sqlx::query_as!(DBWarcRecord,
            "SELECT * FROM masstuffy_records WHERE identifier=$1 AND (flags&1) = 1 LIMIT 1", id).fetch_one(&self.db).await?.into();
        Ok(record)
    }

    pub async fn activate_records(&self, collection: &String, dict_id: Option<i64>, dict_type: Option<&str>) -> anyhow::Result<()> {
        sqlx::query!(r#"
        UPDATE masstuffy_records
        SET flags = flags|1
        WHERE
            collection = $1 AND
            dict_id    = $2 AND
            dict_type  = $3"#,
        collection, dict_id, dict_type)
            .execute(&self.db).await?;
        Ok(())
    }

    pub async fn delete_records(&self, collection: &String, dict_id: Option<i64>, dict_type: Option<&str>) -> anyhow::Result<()> {
        sqlx::query!(r#"
        DELETE FROM masstuffy_records
        WHERE
            collection = $1 AND
            dict_id    = $2 AND
            dict_type  = $3"#,
            collection, dict_id, dict_type)
            .execute(&self.db).await?;
        Ok(())
    }

    pub async fn get_record_from_uri(&self, date: &String, uri: &String) -> anyhow::Result<DBWarcRecord> {
        // TOOD: better way than comparing epoches?
        let record: DBWarcRecord = sqlx::query_as!(DBWarcRecord,
            r#"SELECT * FROM masstuffy_records
            WHERE
                "type" != 'request' AND
                uri=$1 AND
                (flags&1) = 1
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
            AND (flags&1) = 1
            ORDER BY hashint8(id)
            LIMIT $2"#, collection, limit).
            fetch_all(&self.db).await?) // TODO: make it random?
    }

    pub async fn search(&self, 
        host: Match,
        port: Option<u16>,
        path: Match,
        limit: i64) -> anyhow::Result<Vec<DBWarcRecord>> {
        let pattern = massaged_url_pattern(host, port, path);

        Ok(sqlx::query_as!(
            DBWarcRecord,
            r#"
            SELECT * FROM masstuffy_records
            WHERE massaged_url ~ $1
            LIMIT $2"#, pattern, limit).
            fetch_all(&self.db).await?)
    }

    pub async fn delete_collection(&self, collection: &String) -> anyhow::Result<()> {
        sqlx::query!(
        r#"
        DELETE FROM masstuffy_records
        WHERE collection = $1
        "#, collection).execute(&self.db).await?;
        Ok(())
    }

    pub async fn get_permissions(&self, token: &str) -> anyhow::Result<Option<TokenInfo>> {
        Ok(sqlx::query_as!(
            DBToken,
            r#"
            SELECT * FROM masstuffy_tokens
            WHERE token = $1
            LIMIT 1"#, token).
            fetch_optional(&self.db).await?.map(|t| TokenInfo::from_db_row(t)))
    }

    pub async fn delete_permissions(&self, token: &String) -> anyhow::Result<()> {
        sqlx::query_as!(
            DBToken,
            r#"DELETE FROM masstuffy_tokens WHERE token = $1"#, token).
            execute(&self.db).await?;
        Ok(())
    }

    pub async fn get_all_permissions(&self) -> anyhow::Result<Vec<TokenInfo>> {
        Ok(sqlx::query_as!(
            DBToken,
            "SELECT * FROM masstuffy_tokens").
            fetch_all(&self.db).await?
                .into_iter().map(|t| TokenInfo::from_db_row(t)).collect())
    }

    pub async fn create_permissions(&self, perms: TokenInfo) -> anyhow::Result<()> {
        sqlx::query!(r#"
        INSERT INTO masstuffy_tokens(
                token, comment,
                read_perms_kind, read_perms,
                write_perms_kind, write_perms,
                delete_perms_kind, delete_perms)
        VALUES(
            $1, $2,
            $3, $4,
            $5, $6,
            $7, $8)"#,
            perms.token, perms.comment,
            perms.read_perms.get_perms_kind(), perms.read_perms.get_perms(),
            perms.write_perms.get_perms_kind(), perms.write_perms.get_perms(),
            perms.delete_perms.get_perms_kind(), perms.delete_perms.get_perms())
            .execute(&self.db).await?;
        Ok(())
    }
}