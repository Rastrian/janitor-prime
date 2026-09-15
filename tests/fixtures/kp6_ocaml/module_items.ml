(* KP-6-ocaml curated pattern 3a: module-item adjacency (.ml). Module
   bodies are `structure` nodes whose items are true siblings; the
   comment between make and classify is the byte-strict loss. *)

module Widget = struct
  type t = int

  let make x = x

  let describe x = "w"

  (* between module items, documented late *)
  let classify x = x mod 2
end
