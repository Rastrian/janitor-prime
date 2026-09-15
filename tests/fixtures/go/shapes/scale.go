// Package shapes normalizes and folds product codes.
package shapes

import (
	"fmt"
	"strings"
)

// NormalizeCode is the funnel every later phase passes through: trim,
// lower, and hand back one canonical token.
func NormalizeCode(code string) string {
	return strings.ToLower(strings.TrimSpace(code))
}

func squashSuffix(code string) string {
	return strings.TrimSuffix(code, "-x")
}

// ExpandCode re-adds the suffix squashSuffix removes (round-trip helper).
func ExpandCode(code string) string {
	return code + "-x"
}

// Version pins the schema the folder understands.
type Version int

// Apply is a method (a method_declaration, not a function_declaration):
// its name child is a field_identifier, so name-capturing rules must
// scope to the declaration kind they mean.
func (v Version) Apply(code string) string {
	return NormalizeCode(fmt.Sprintf("v%d-%s", int(v), code))
}
