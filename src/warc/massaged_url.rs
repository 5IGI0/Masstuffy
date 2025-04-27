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
 
use url::Url;
use anyhow::Result;
use urlencoding::encode;
use std::fmt::Write;
 
fn default_port_from_scheme(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None
    }
}

/* NOTE:    this function is not exactly in line with the webarchive's massaged urls.
            it differs by adding an & at the beginning and end of the query
            this allows for a better LIKE search in sql */
pub fn massage_url(url: &str) -> Result<String> {
    let mut ret = String::new();
    let url = Url::parse(url)?;

    /* massaged host or ip */
    if let Some(domain) = url.domain() {
        let mut splitted_domain: Vec<String> = domain.trim_matches('.').split(".").into_iter().map(|x| x.to_string()).collect();
        splitted_domain.reverse();
        ret.write_str(&splitted_domain.join(","))?;
    } else if let Some(host) = url.host_str() {
        ret.write_str(host)?;
    }

    if let Some(port) = url.port() {
        let def_port = default_port_from_scheme(url.scheme());

        if def_port.is_none() || def_port.unwrap() != port {
            ret.write_char(':')?;
            ret.write_fmt(format_args!("{}", port))?;
        }
    }

    ret.write_char(')')?;
    ret.write_str(url.path())?;

    let mut pairs: Vec<(String,String)> = url.query_pairs().map(|p| (p.0.to_string(), encode(&p.1).to_string())).collect();
    if pairs.len() != 0 {
        ret.write_str("?&")?;
        pairs.sort();
        for (key, val) in &pairs {
            ret.write_str(key)?;
            ret.write_char('=')?;
            ret.write_str(val)?;
            ret.write_char('&')?;
        }
    }

    Ok(ret)
}
