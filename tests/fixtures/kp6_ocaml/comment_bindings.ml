(* KP-6-ocaml curated pattern 2: comment-between-bindings. Top-level
   lets ARE true siblings inside compilation_unit, and the comment
   between the second and third is what byte-strict loses. *)

let one = 1

let two = 2

(* between bindings, documented late *)
let three = 3
