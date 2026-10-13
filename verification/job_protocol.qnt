module job_protocol {
  var phase: str
  var before: str
  var command: str
  var refused: bool

  action init = all {
    phase' = "absent",
    before' = "absent",
    command' = "init",
    refused' = false,
  }

  action submit_same = all {
    before' = phase,
    command' = "submit_same",
    refused' = false,
    phase' = if (phase == "absent") "accepted" else phase,
  }

  action submit_conflict = all {
    before' = phase,
    command' = "submit_conflict",
    refused' = phase != "absent",
    phase' = if (phase == "absent") "accepted" else phase,
  }

  action attach = all {
    before' = phase,
    command' = "attach",
    refused' = phase == "absent",
    phase' = if (phase == "accepted") "active" else phase,
  }

  action disconnect = all {
    before' = phase,
    command' = "disconnect",
    refused' = false,
    phase' = phase,
  }

  action reap = all {
    before' = phase,
    command' = "reap",
    refused' = phase != "active",
    phase' = if (phase == "active") "finished" else phase,
  }

  action step = any { submit_same, submit_conflict, attach, disconnect, reap }

  val durable = all {
    Set("absent", "accepted", "active", "finished").contains(phase),
    (command == "disconnect") implies (phase == before),
    refused implies (phase == before),
    (before == "finished") implies (phase == "finished"),
  }

  val inductive = all {
    Set("absent", "accepted", "active", "finished").contains(phase),
    Set("absent", "accepted", "active", "finished").contains(before),
    Set("init", "submit_same", "submit_conflict", "attach", "disconnect", "reap").contains(command),
    Set(true, false).contains(refused),
    durable,
  }
}
