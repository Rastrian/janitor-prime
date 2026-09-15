// KP-6-go curated snippet: method-with-receiver adjacency.
//
// The method_declaration's leading named children are the receiver
// parameter_list then the method's field_identifier name. Same-line
// comments are legal go between them (semicolon insertion only fires at
// line ends), so a /* role */ note there is a named sibling that breaks
// byte-strict adjacency exactly like a line comment does elsewhere.

package kp6

type Counter int

func (c Counter) Plain() Counter { return c }

func (c Counter) /* role: folding */ Annotated() Counter { return c + 1 }
