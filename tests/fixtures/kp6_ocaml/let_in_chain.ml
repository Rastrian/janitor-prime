(* KP-6-ocaml curated pattern 1: let..in chains. Measured surprise: a
   chain is NOT a sibling list — each `let ... in` wraps the rest of the
   chain as its body, so successive let-bindings are parent-and-child
   (let_expression > value_definition, let_expression > ...), never
   siblings in either trivia mode. *)

let chain x =
  let a = x + 1 in
  let b = a * 2 in
  a + b
