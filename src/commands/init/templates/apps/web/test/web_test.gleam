import gleeunit
import shared/api
import web/app

pub fn main() {
  gleeunit.main()
}

pub fn successful_health_response_connects_test() {
  let response = api.HealthResponse(status: "ok", service: "api")
  let #(model, _) = app.update(app.Loading, app.ApiReturnedHealth(Ok(response)))

  assert model == app.Connected(response)
}
