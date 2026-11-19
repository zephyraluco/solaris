use super::{McpServer, invalid};
use agent_client_protocol::{Error, v1::schema::ImageContent};
use base64::{Engine, engine::general_purpose::STANDARD};
use solaris_mcp::{McpConfig, ServerConfig};
use solaris_tools::InlineImage;

pub(super) fn image(image: ImageContent) -> Result<InlineImage, Error> {
    let limit = solaris_tools::MAX_IMAGE_BYTES;
    if image.data.len() > limit.div_ceil(3) * 4 {
        return Err(invalid("image exceeds 8 MiB"));
    }
    let bytes = STANDARD
        .decode(&image.data)
        .map_err(|_| invalid("invalid base64 image"))?;
    let validated = solaris_tools::image_from_bytes(&bytes)
        .map_err(invalid)?
        .ok_or_else(|| invalid("unsupported image format"))?;
    if validated.media_type != image.mime_type {
        return Err(invalid("image MIME type does not match its bytes"));
    }
    Ok(validated)
}

pub(super) fn merge_mcp(
    mut config: McpConfig,
    servers: Vec<McpServer>,
) -> Result<McpConfig, Error> {
    for server in servers {
        let (name, server) = match server {
            McpServer::Stdio(server) => {
                if !server.command.is_absolute() {
                    return Err(invalid("client MCP executable must be an absolute path"));
                }
                (
                    server.name,
                    ServerConfig::ClientStdio {
                        command: server.command,
                        args: server.args,
                        env: server
                            .env
                            .into_iter()
                            .map(|entry| (entry.name, entry.value))
                            .collect(),
                    },
                )
            }
            McpServer::Http(server) => (
                server.name,
                ServerConfig::ClientHttp {
                    url: server.url,
                    headers: server
                        .headers
                        .into_iter()
                        .map(|header| (header.name, header.value))
                        .collect(),
                },
            ),
            _ => return Err(invalid("client MCP supports stdio and streamable HTTP")),
        };
        if config.servers.contains_key(&name) {
            return Err(invalid(format!("duplicate MCP server name: {name}")));
        }
        config.servers.insert(name, server);
    }
    Ok(config)
}
