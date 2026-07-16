import lustre/attribute
import lustre/effect.{type Effect}
import lustre/element.{type Element}
import lustre/element/html
import lustre/event
import rsvp
import shared/api

pub type Model {
  Loading
  Connected(api.HealthResponse)
  Failed
}

pub type Msg {
  ApiReturnedHealth(Result(api.HealthResponse, rsvp.Error(String)))
  UserClickedRetry
}

pub fn init(_flags: Nil) -> #(Model, Effect(Msg)) {
  #(Loading, load_health())
}

pub fn update(_model: Model, message: Msg) -> #(Model, Effect(Msg)) {
  case message {
    ApiReturnedHealth(Ok(response)) -> #(Connected(response), effect.none())
    ApiReturnedHealth(Error(_)) -> #(Failed, effect.none())
    UserClickedRetry -> #(Loading, load_health())
  }
}

fn load_health() -> Effect(Msg) {
  let handler =
    rsvp.expect_json(api.health_response_decoder(), ApiReturnedHealth)

  rsvp.get(api.health_path, handler)
}

pub fn view(model: Model) -> Element(Msg) {
  html.main([attribute.class("shell")], [
    html.section([attribute.class("card")], [
      html.p([attribute.class("eyebrow")], [html.text("GOMO + LUSTRE")]),
      html.h1([], [html.text("Your full-stack Gleam app is ready.")]),
      html.p([attribute.class("intro")], [
        html.text(
          "The browser, shared contract, and API service are connected through one typed workspace.",
        ),
      ]),
      status(model),
    ]),
  ])
}

fn status(model: Model) -> Element(Msg) {
  case model {
    Loading ->
      html.div([attribute.class("status loading")], [
        html.span([attribute.class("dot")], []),
        html.text("Connecting to the API..."),
      ])
    Connected(response) ->
      html.div([attribute.class("status connected")], [
        html.span([attribute.class("dot")], []),
        html.span([], [
          html.text(response.service <> " responded with " <> response.status),
        ]),
      ])
    Failed ->
      html.div([attribute.class("error")], [
        html.p([], [html.text("The API could not be reached.")]),
        html.button([event.on_click(UserClickedRetry)], [html.text("Try again")]),
      ])
  }
}
