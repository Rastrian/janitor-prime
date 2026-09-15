(* A store of keyed entries, in the shape real ocaml services grow. *)

type entry =
  { key : string
  ; value : int
  }

let normalize_code input =
  let cleaned = String.lowercase_ascii (String.trim input) in
  let squashed =
    String.map (fun c -> if c = ' ' then '_' else c) cleaned
  in
  squashed

let entry_key entry = normalize_code entry.key

(* Rendered summaries run through the same funnel. *)
let render entry =
  Printf.sprintf "%s: %d" (normalize_code entry.key) entry.value

let total entries = List.fold_left (fun acc e -> acc + e.value) 0 entries
