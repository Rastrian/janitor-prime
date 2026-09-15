package shapes

import (
	"strings"
)

// FoldCode folds two codes through the package normalizer.
func FoldCode(first, second string) string {
	joined := NormalizeCode(first) + "/" + NormalizeCode(second)
	return strings.Trim(joined, "/")
}

func foldAll(codes []string) []string {
	out := make([]string, 0, len(codes))
	for _, code := range codes {
		out = append(out, FoldCode(code, code))
	}
	return out
}

// Describe renders the folded corpus for humans.
func Describe(codes []string) string {
	return strings.Join(foldAll(codes), ", ")
}
