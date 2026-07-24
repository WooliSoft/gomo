import shared/api
import unitest
import web/app

pub fn main() {
  unitest.run(
    unitest.Options(
      ..unitest.default_options(),
      execution_mode: unitest.RunParallelAuto,
    ),
  )
}

pub fn successful_health_response_connects_test() {
  let response = api.HealthResponse(status: "ok", service: "api")
  let #(model, _) = app.update(app.Loading, app.ApiReturnedHealth(Ok(response)))

  assert model == app.Connected(response)
}
