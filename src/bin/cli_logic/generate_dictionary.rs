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

use std::error::Error;

use chrono::Utc;
use clap::Parser;
use log::{debug, error, info};
use masstuffy::{constants::MASSTUFFY_DATE_FMT, database::DBManager, filesystem};
use rand::RngCore;

#[derive(Parser)]
struct Args {
    collection: String,

    /// number of sample
    #[arg(short, long, default_value_t=100000)]
    num_sample: i64,

    /// max dictionnary size
    #[arg(short, long, default_value_t=5000000)]
    max_dict_size: usize,
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let mut fs = filesystem::init().await?;
    let db = DBManager::new(&fs.get_database_conn_string());

    info!("creating buffer...");
    let (path, exists) = fs.get_buffer_path(&format!("gen_{}_dict", args.collection), true).await?;

    if exists {
        error!("a buffer already exists (are you doing it twice?)");
        return Ok(1);
    }

    info!("picking samples...");
    let warcs = db.get_samples(&args.collection, args.num_sample).await?;

    if warcs.len() == 0 {
        error!("no sample found");
        return Ok(1);
    }

    {
        // TODO: optimise
        let mut count: u64 = 0;
        for s in warcs.iter() {
            if (count%1000) == 0 {
                info!("copying samples to the buffer ({}/{})...", count, warcs.len());
            }
            count+= 1;

            debug!("copying {} ({})", s.id, s.identifier);
            let content = fs.get_record(&args.collection, &s.filename, s.offset).await?.
                expect(&format!("record {} not found", s.identifier)).serialize();
            tokio::fs::write(format!("{}/{}", path, s.id), &content[..]).await?
        }
    }

    let mut dict = zstd::dict::from_files(
        warcs.iter().
            map(|r| format!("{}/{}", path, r.id)),
        args.max_dict_size)?;

    let mut rng = rand::rng();
    loop {
        let dict_id = u32::from_le_bytes(dict[4..8].try_into()?);

        debug!("dictionary id: {}", dict_id);
        if !fs.has_zstd_dict(dict_id).await {
            break;
        }

        debug!("id {} is already used, picking another one...", dict_id);
        /* generate random ids outside of reserved ranges */
        let new_id = (rng.next_u32() % (0x80000000 - 32768)) + 32768;
        dict[4..8].copy_from_slice(&new_id.to_le_bytes());
    }

    tokio::fs::remove_dir_all(path).await?;
    fs.add_zstd_dict(&format!("{}_{}", args.collection, Utc::now().format(MASSTUFFY_DATE_FMT)), dict).await;

    Ok(0)
}