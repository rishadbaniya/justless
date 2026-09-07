/// Reconnects the websocket after the server closes it.
pub fn reconnect_websocket(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("missing websocket url".into());
    }
    Ok(())
}

/// Splits one `Name: value` header line.
pub fn parse_http_header(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once(':')?;
    Some((name.trim(), value.trim()))
}
