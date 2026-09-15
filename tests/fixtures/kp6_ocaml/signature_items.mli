(* KP-6-ocaml curated pattern 3b: signature-item adjacency (.mli). At
   interface level the compilation_unit's items are the signature's true
   siblings; the comment between the two val specifications is the
   byte-strict loss. *)

type t = int

val render : t -> string

(* between signature items, documented late *)
val tally : t -> int
