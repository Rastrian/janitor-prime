// KP-6-typescript curated snippet: type-annotation siblings.
//
// Inside a variable_declarator the declared identifier and its
// type_annotation are named siblings; a comment between the name and
// the annotation is legal typescript (same line or not). Strict sees
// one pair, skip sees both.

const width: number = 12;

const label /* display-only */ : string = "px";
