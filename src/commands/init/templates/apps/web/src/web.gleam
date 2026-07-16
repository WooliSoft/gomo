import lustre
import web/app

pub fn main() {
  let application = lustre.application(app.init, app.update, app.view)
  let assert Ok(_) = lustre.start(application, "#app", Nil)

  Nil
}
