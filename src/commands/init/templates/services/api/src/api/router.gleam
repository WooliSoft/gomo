import gleam/http.{Get}
import gleam/json
import shared/api as contract
import wisp.{type Request, type Response}

pub fn handle_request(static_directory: String, request: Request) -> Response {
  use <- wisp.log_request(request)
  use request <- wisp.handle_head(request)

  case request.method, wisp.path_segments(request) {
    Get, ["api", "health"] -> health()
    Get, [] -> wisp.redirect(to: "/index.html")
    _, _ -> {
      use <- wisp.serve_static(request, under: "", from: static_directory)
      wisp.not_found()
    }
  }
}

fn health() -> Response {
  contract.HealthResponse(status: "ok", service: "api")
  |> contract.health_response_to_json
  |> json.to_string
  |> wisp.json_response(200)
}
