use std::io;

#[expect(
    clippy::disallowed_methods,
    reason = "build configuration is parsed only at this source-stamp boundary"
)]
#[expect(
    clippy::redundant_pub_crate,
    reason = "the parser is shared by the build script and the library"
)]
pub(crate) fn rust_tool(bytes: &[u8]) -> io::Result<String> {
    let source = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let document: toml::Value = toml::from_str(source).map_err(io::Error::other)?;
    let rust = document
        .get("tools")
        .and_then(|tools| tools.get("rust"))
        .ok_or_else(|| io::Error::other("mise.toml has no tools.rust setting"))?;
    Ok(rust.to_string())
}
