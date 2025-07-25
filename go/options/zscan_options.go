// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

package options

import "github.com/valkey-io/valkey-glide/go/v2/constants"

// This struct represents the optional arguments for the ZSCAN command.
type ZScanOptions struct {
	BaseScanOptions
	NoScores bool
}

func NewZScanOptions() *ZScanOptions {
	return &ZScanOptions{}
}

// SetNoScores sets the noScores flag for the ZSCAN command.
// If this value is set to true, the ZSCAN command will be called with NOSCORES option.
// In the NOSCORES option, scores are not included in the response.
// Supported from Valkey 8.0.0 and above.
func (zScanOptions *ZScanOptions) SetNoScores(noScores bool) *ZScanOptions {
	zScanOptions.NoScores = noScores
	return zScanOptions
}

// SetMatch sets the match pattern for the ZSCAN command.
func (zScanOptions *ZScanOptions) SetMatch(match string) *ZScanOptions {
	zScanOptions.BaseScanOptions.SetMatch(match)
	return zScanOptions
}

// SetCount sets the count of the ZSCAN command.
func (zScanOptions *ZScanOptions) SetCount(count int64) *ZScanOptions {
	zScanOptions.BaseScanOptions.SetCount(count)
	return zScanOptions
}

func (options *ZScanOptions) ToArgs() ([]string, error) {
	args := []string{}
	baseArgs, err := options.BaseScanOptions.ToArgs()
	args = append(args, baseArgs...)

	if options.NoScores {
		args = append(args, constants.NoScoresKeyword)
	}
	return args, err
}
