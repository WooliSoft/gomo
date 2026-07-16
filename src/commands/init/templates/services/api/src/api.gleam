import api/router
import gleam/erlang/process
import gleam/io
import mist
import wisp
import wisp/wisp_mist

pub fn main() {
  wisp.configure_logger()
  let secret_key_base = wisp.random_string(64)
  let assert Ok(priv_directory) = wisp.priv_directory("api")
  let static_directory = priv_directory <> "/static"

  let assert Ok(_) =
    router.handle_request(static_directory, _)
    |> wisp_mist.handler(secret_key_base)
    |> mist.new
    |> mist.port(3000)
    |> mist.start

  io.println("API listening on http://localhost:3000")
  process.sleep_forever()
}
