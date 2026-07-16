import gleam/dynamic/decode
import gleam/json

pub const health_path = "/api/health"

pub type HealthResponse {
  HealthResponse(status: String, service: String)
}

pub fn health_response_to_json(response: HealthResponse) -> json.Json {
  json.object([
    #("status", json.string(response.status)),
    #("service", json.string(response.service)),
  ])
}

pub fn health_response_decoder() -> decode.Decoder(HealthResponse) {
  use status <- decode.field("status", decode.string)
  use service <- decode.field("service", decode.string)
  decode.success(HealthResponse(status:, service:))
}
