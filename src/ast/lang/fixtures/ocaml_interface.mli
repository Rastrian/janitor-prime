(* Bundled ocaml interface fixture corpus (E5 disposition): the fixed
   body of real-shape signature code the LangRegistry seeds its kind
   table from at first use. Parsed only; never executed. *)

type kind = Alpha | Beta | Gamma

type t =
  { id : int
  ; kind : kind
  }

(* The construction and classification surface. *)
val make : int -> kind -> t

val classify : t -> string option

val tally : (int * int) list -> int

module Widget : sig
  type internal

  val expose : internal -> string
end
