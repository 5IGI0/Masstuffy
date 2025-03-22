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
    pub r#type: String
}