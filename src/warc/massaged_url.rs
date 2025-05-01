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

pub fn massage_url(url: &str) -> Result<String> {
    let mut ret = String::new();
    let url = Url::parse(url)?;

    /* massaged host or ip */
    if let Some(domain) = url.domain() {
        ret.write_str(&domain2massaged(domain))?;
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
        ret.write_str("?")?;
        let mut is_first = true;
        pairs.sort();
        for (key, val) in &pairs {
            if !is_first {
                ret.write_char('&')?;
            }
            is_first = false;
            ret.write_str(key)?;
            ret.write_char('=')?;
            ret.write_str(val)?;
        }
    }

    Ok(ret)
}

pub enum Match {
    None,
    ExactMatch(String),
    PartialMatch(String)
}

// TODO: check input values
pub fn massaged_url_pattern(
    host: Match,
    port: Option<u16>,
    path: Match
) -> String {
    let mut ret = String::new();

    // TODO: it's assuming host is a domain (must detect IPs)
    match &host {
        Match::None => ret.write_str(".*").unwrap(),
        Match::ExactMatch(d) => ret.write_str(&domain2massaged(d)).unwrap(),
        Match::PartialMatch(d) => ret.write_fmt(format_args!("{}{}", domain2massaged(d), "(,[a-z0-9]+){0,}")).unwrap()
    }

    match port {
        Some(port) => ret.write_fmt(format_args!(":{port}")).unwrap(),
        None => if !ret.ends_with(".*") {ret.write_str("(:[0-9]{1,5})?").unwrap()}
    }

    if !ret.ends_with(".*") {
        ret.write_str("\\)").unwrap();
    }

    // TODO: escape path
    match &path {
        Match::None => if !ret.ends_with(".*") {ret.write_str(".*").unwrap()},
        Match::ExactMatch(p) => ret.write_str(p).unwrap(),
        Match::PartialMatch(p) => ret.write_fmt(format_args!("{}.*", p)).unwrap()
    }

    // TODO: GET parameters.

    ret
}

// TODO: IDNA encode
fn domain2massaged(domain: &str) -> String {
    let mut splitted_domain: Vec<String> = domain.trim_matches('.').split(".").into_iter().map(|x| x.to_string()).collect();
    splitted_domain.reverse();
    return splitted_domain.join(",");
}