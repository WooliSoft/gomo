import gleam/json
import gleeunit
import shared/api

pub fn main() {
  gleeunit.main()
}

pub fn health_response_round_trip_test() {
  let expected = api.HealthResponse(status: "ok", service: "api")
  let encoded = expected |> api.health_response_to_json |> json.to_string
  let assert Ok(decoded) = json.parse(encoded, api.health_response_decoder())

  assert decoded == expected
}
