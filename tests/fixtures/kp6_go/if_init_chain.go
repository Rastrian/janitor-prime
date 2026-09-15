// KP-6-go curated snippet: if/init-statement sibling chains.
//
// Three if statements as siblings in the body's statement_list, the
// first two with the `v, ok :=` init-statement shape (named children
// short_var_declaration, identifier, block of the if itself). The third
// sits behind a section comment: strict adjacency ends at the second if,
// skip reaches the third.

package kp6

func Chain(m map[string]bool) {
	if v, ok := m["a"]; ok {
		_ = v
	}
	if v, ok := m["b"]; ok {
		_ = v
	}
	// everything below the fold is optional
	if v, ok := m["c"]; ok {
		_ = v
	}
}
