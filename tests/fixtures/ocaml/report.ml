(* A report over the store, let..in-heavy on purpose — the KP-6 chain
   facts are visible in this file. *)

let line entry =
  let key = Store.entry_key entry in
  let rendered = Store.render entry in
  key ^ " -> " ^ rendered

let summary entries =
  let lines = List.map (fun entry -> line entry) entries in
  let joined = String.concat "\n" lines in
  "summary:\n" ^ joined

let () = print_endline (summary [])
