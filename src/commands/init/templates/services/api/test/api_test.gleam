import api/router
import gleam/http.{Get}
import gleam/json
import gleeunit
import shared/api as contract
import wisp/simulate

pub fn main() {
  gleeunit.main()
}

pub fn health_route_test() {
  let response =
    router.handle_request("unused", simulate.request(Get, contract.health_path))
  let assert Ok(body) =
    json.parse(simulate.read_body(response), contract.health_response_decoder())

  assert response.status == 200
  assert body == contract.HealthResponse(status: "ok", service: "api")
}
