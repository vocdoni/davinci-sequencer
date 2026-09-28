// Vector generator for the Rust arbo port: replays random ops on Go arbo
// and records every result so the Rust side can diff byte for byte.
// Run with -bench for the Go-side benchmark workloads instead.
package main

import (
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"time"

	"github.com/vocdoni/arbo"
	"github.com/vocdoni/arbo/memdb"
	"github.com/vocdoni/davinci-node/db"
	"github.com/vocdoni/davinci-node/db/pebbledb"
)

type circomProof struct {
	Root     string   `json:"root"`
	Siblings []string `json:"siblings"`
	OldKey   string   `json:"old_key"`
	OldValue string   `json:"old_value"`
	IsOld0   bool     `json:"is_old0"`
	Key      string   `json:"key"`
	Value    string   `json:"value"`
	Fnc      int      `json:"fnc"`
}

type opRecord struct {
	Op        string       `json:"op"`
	Key       string       `json:"key"`
	Value     string       `json:"value,omitempty"`
	Err       bool         `json:"err,omitempty"`
	Root      string       `json:"root,omitempty"`
	Found     bool         `json:"found,omitempty"`
	LeafKey   string       `json:"leaf_key,omitempty"`
	LeafValue string       `json:"leaf_value,omitempty"`
	Packed    string       `json:"packed,omitempty"`
	Existence bool         `json:"existence,omitempty"`
	Check     bool         `json:"check,omitempty"`
	Circom    *circomProof `json:"circom,omitempty"`
}

type vectorFile struct {
	MaxLevels    int        `json:"max_levels"`
	Seed         int64      `json:"seed"`
	Ops          []opRecord `json:"ops"`
	FinalRoot    string     `json:"final_root"`
	NLeafs       int        `json:"n_leafs"`
	Dump         string     `json:"dump"`
	BatchN       int        `json:"batch_n"`
	BatchRoot    string     `json:"batch_root"`
	BatchInvalid int        `json:"batch_invalid"`
}

func hx(b []byte) string { return hex.EncodeToString(b) }

func newTree(levels int) *arbo.Tree {
	t, err := arbo.NewTree(arbo.Config{
		Database:     memdb.New(),
		MaxLevels:    levels,
		HashFunction: arbo.HashFunctionSha256,
	})
	if err != nil {
		panic(err)
	}
	return t
}

func mustRoot(t *arbo.Tree) []byte {
	r, err := t.Root()
	if err != nil {
		panic(err)
	}
	return r
}

// circomRecord renders a Go CircomVerifierProof with raw bytes as hex.
func circomRecord(cvp *arbo.CircomVerifierProof) *circomProof {
	sibs := make([]string, len(cvp.Siblings))
	for i, s := range cvp.Siblings {
		sibs[i] = hx(s)
	}
	return &circomProof{
		Root:     hx(cvp.Root),
		Siblings: sibs,
		OldKey:   hx(cvp.OldKey),
		OldValue: hx(cvp.OldValue),
		IsOld0:   cvp.IsOld0,
		Key:      hx(cvp.Key),
		Value:    hx(cvp.Value),
		Fnc:      cvp.Fnc,
	}
}

func genProofRecord(tree *arbo.Tree, key []byte) opRecord {
	rec := opRecord{Op: "gen_proof", Key: hx(key)}
	leafK, leafV, packed, existence, err := tree.GenProof(key)
	if err != nil {
		panic(fmt.Sprintf("GenProof: %v", err))
	}
	rec.LeafKey = hx(leafK)
	rec.LeafValue = hx(leafV)
	rec.Packed = hx(packed)
	rec.Existence = existence
	// CheckProof over the leaf actually found (queried key for existence).
	ck, cv := key, leafV
	if !existence {
		ck, cv = leafK, leafV
	}
	if existence || len(leafK) > 0 {
		ok, err := arbo.CheckProof(arbo.HashFunctionSha256, ck, cv, mustRoot(tree), packed)
		if err != nil {
			panic(err)
		}
		rec.Check = ok
	}
	cvp, err := tree.GenerateCircomVerifierProof(key)
	if err != nil {
		panic(err)
	}
	rec.Circom = circomRecord(cvp)
	return rec
}

// derivedKVs regenerates the deterministic sha256("arbo-batch"...) keys and
// values shared with the Rust tests (differential.rs batch_kvs).
func derivedKVs(levels, seed, n int) ([][]byte, [][]byte) {
	keyLen := (levels + 7) / 8
	keys := make([][]byte, n)
	vals := make([][]byte, n)
	for i := range keys {
		input := []byte("arbo-batch")
		var b [12]byte
		binary.LittleEndian.PutUint32(b[0:4], uint32(levels))
		binary.LittleEndian.PutUint32(b[4:8], uint32(seed))
		binary.LittleEndian.PutUint32(b[8:12], uint32(i))
		h := sha256.Sum256(append(input, b[:]...))
		v := sha256.Sum256(h[:])
		keys[i] = append([]byte{}, h[:keyLen]...)
		vals[i] = v[:]
	}
	return keys, vals
}

type batchDiskVector struct {
	MaxLevels    int    `json:"max_levels"`
	BaseSeed     int    `json:"base_seed"`
	BaseN        int    `json:"base_n"`
	BatchSeed    int    `json:"batch_seed"`
	BatchN       int    `json:"batch_n"`
	BaseRoot     string `json:"base_root"`
	FinalRoot    string `json:"final_root"`
	BaseInvalid  int    `json:"base_invalid"`
	BatchInvalid int    `json:"batch_invalid"`
}

// genBatchDiskVector records AddBatch into a populated tree above Go's
// 65536-leaf disk threshold, exercising addBatchInDisk. Pebble only:
// memdb's addBatchInDisk panics ("unsupported WriteTx type"). The kvs are
// derived, so the JSON carries just seeds and roots.
func genBatchDiskVector(outDir string) {
	dir, err := os.MkdirTemp("", "arbodisk")
	if err != nil {
		panic(err)
	}
	database, err := pebbledb.New(db.Options{Path: dir})
	if err != nil {
		panic(err)
	}
	t, err := arbo.NewTree(arbo.Config{
		Database: database, MaxLevels: 64,
		HashFunction: arbo.HashFunctionSha256,
	})
	if err != nil {
		panic(err)
	}

	vf := batchDiskVector{
		MaxLevels: 64, BaseSeed: 7, BaseN: 70000, BatchSeed: 8, BatchN: 1000,
	}
	baseKeys, baseVals := derivedKVs(vf.MaxLevels, vf.BaseSeed, vf.BaseN)
	inv, err := t.AddBatch(baseKeys, baseVals)
	if err != nil {
		panic(err)
	}
	vf.BaseInvalid = len(inv)
	vf.BaseRoot = hx(mustRoot(t))

	batchKeys, batchVals := derivedKVs(vf.MaxLevels, vf.BatchSeed, vf.BatchN)
	inv, err = t.AddBatch(batchKeys, batchVals)
	if err != nil {
		panic(err)
	}
	vf.BatchInvalid = len(inv)
	vf.FinalRoot = hx(mustRoot(t))

	if err := database.Close(); err != nil {
		panic(err)
	}
	if err := os.RemoveAll(dir); err != nil {
		panic(err)
	}

	out, err := json.MarshalIndent(vf, "", " ")
	if err != nil {
		panic(err)
	}
	name := filepath.Join(outDir, "sha256_batch_disk.json")
	if err := os.WriteFile(name, out, 0o644); err != nil {
		panic(err)
	}
	fmt.Printf("wrote %s\n", name)
}

func genVectors(outDir string) {
	for _, levels := range []int{64, 160, 256} {
		keyLen := (levels + 7) / 8
		for seed := int64(1); seed <= 3; seed++ {
			rng := rand.New(rand.NewSource(seed*1000 + int64(levels)))
			tree := newTree(levels)
			var inserted [][]byte
			var ops []opRecord

			randKey := func() []byte {
				k := make([]byte, keyLen)
				rng.Read(k)
				return k
			}
			randVal := func() []byte {
				v := make([]byte, 32)
				rng.Read(v)
				return v
			}
			// derivedKey flips one bit of an existing key, biased to high
			// bit positions to force deep shared prefixes (floating leaves).
			derivedKey := func() []byte {
				base := inserted[rng.Intn(len(inserted))]
				k := append([]byte(nil), base...)
				var bit int
				if rng.Intn(2) == 0 {
					bit = levels - 1 - rng.Intn(levels/8)
				} else {
					bit = rng.Intn(levels)
				}
				k[bit/8] ^= 1 << (bit % 8)
				return k
			}

			for i := 0; i < 200; i++ {
				r := rng.Intn(100)
				switch {
				case r < 45 || len(inserted) == 0: // add new key
					var k []byte
					if len(inserted) > 0 && rng.Intn(4) == 0 {
						k = derivedKey()
					} else {
						k = randKey()
					}
					v := randVal()
					err := tree.Add(k, v)
					if err == nil {
						inserted = append(inserted, k)
					}
					ops = append(ops, opRecord{
						Op: "add", Key: hx(k), Value: hx(v),
						Err: err != nil, Root: hx(mustRoot(tree)),
					})
				case r < 55: // add existing key (error)
					k := inserted[rng.Intn(len(inserted))]
					v := randVal()
					err := tree.Add(k, v)
					ops = append(ops, opRecord{
						Op: "add", Key: hx(k), Value: hx(v),
						Err: err != nil, Root: hx(mustRoot(tree)),
					})
				case r < 65: // update existing
					k := inserted[rng.Intn(len(inserted))]
					v := randVal()
					err := tree.Update(k, v)
					ops = append(ops, opRecord{
						Op: "update", Key: hx(k), Value: hx(v),
						Err: err != nil, Root: hx(mustRoot(tree)),
					})
				case r < 70: // update missing (error)
					k := randKey()
					v := randVal()
					err := tree.Update(k, v)
					ops = append(ops, opRecord{
						Op: "update", Key: hx(k), Value: hx(v),
						Err: err != nil, Root: hx(mustRoot(tree)),
					})
				case r < 80: // get existing
					k := inserted[rng.Intn(len(inserted))]
					leafK, leafV, err := tree.Get(k)
					ops = append(ops, opRecord{
						Op: "get", Key: hx(k), Found: err == nil,
						LeafKey: hx(leafK), LeafValue: hx(leafV),
					})
				case r < 85: // get missing
					k := randKey()
					leafK, leafV, err := tree.Get(k)
					ops = append(ops, opRecord{
						Op: "get", Key: hx(k), Found: err == nil,
						LeafKey: hx(leafK), LeafValue: hx(leafV),
					})
				case r < 95: // proof of existing key
					k := inserted[rng.Intn(len(inserted))]
					ops = append(ops, genProofRecord(tree, k))
				default: // proof of missing key
					var k []byte
					if rng.Intn(2) == 0 && len(inserted) > 0 {
						k = derivedKey()
					} else {
						k = randKey()
					}
					if _, _, err := tree.Get(k); err == nil {
						k = randKey() // derived key happened to exist
					}
					ops = append(ops, genProofRecord(tree, k))
				}
			}

			nLeafs, err := tree.GetNLeafs()
			if err != nil {
				panic(err)
			}
			dump, err := tree.Dump(nil)
			if err != nil {
				panic(err)
			}
			// sanity: importing the dump into a fresh tree gives the same root
			imp := newTree(levels)
			if err := imp.ImportDump(dump); err != nil {
				panic(err)
			}
			if hx(mustRoot(imp)) != hx(mustRoot(tree)) {
				panic("import dump root mismatch")
			}

			// 10k-leaf AddBatch on a fresh tree. Keys/values are
			// sha256-derived so the Rust tests can regenerate them.
			batchN := 10000
			bt := newTree(levels)
			keys, vals := derivedKVs(levels, int(seed), batchN)
			invalids, err := bt.AddBatch(keys, vals)
			if err != nil {
				panic(err)
			}

			vf := vectorFile{
				MaxLevels:    levels,
				Seed:         seed,
				Ops:          ops,
				FinalRoot:    hx(mustRoot(tree)),
				NLeafs:       nLeafs,
				Dump:         hx(dump),
				BatchN:       batchN,
				BatchRoot:    hx(mustRoot(bt)),
				BatchInvalid: len(invalids),
			}
			out, err := json.MarshalIndent(vf, "", " ")
			if err != nil {
				panic(err)
			}
			name := filepath.Join(outDir, fmt.Sprintf("sha256_%d_%d.json", levels, seed))
			if err := os.WriteFile(name, out, 0o644); err != nil {
				panic(err)
			}
			fmt.Printf("wrote %s (%d ops, %d leafs)\n", name, len(ops), nLeafs)
		}
	}
}

// benchmarks: the same workloads benches/tree.rs runs in Rust.
func runBench() {
	levels := 64
	keyLen := 8
	rng := rand.New(rand.NewSource(42))
	mkKVs := func(n int) ([][]byte, [][]byte) {
		keys := make([][]byte, n)
		vals := make([][]byte, n)
		for i := range keys {
			keys[i] = make([]byte, keyLen)
			rng.Read(keys[i])
			vals[i] = make([]byte, 32)
			rng.Read(vals[i])
		}
		return keys, vals
	}

	pebbleTree := func(dir string) (*arbo.Tree, db.Database) {
		database, err := pebbledb.New(db.Options{Path: dir})
		if err != nil {
			panic(err)
		}
		t, err := arbo.NewTree(arbo.Config{
			Database: database, MaxLevels: levels,
			HashFunction: arbo.HashFunctionSha256,
		})
		if err != nil {
			panic(err)
		}
		return t, database
	}

	for _, backend := range []string{"memdb", "pebble"} {
		mk := func() (*arbo.Tree, func()) {
			if backend == "memdb" {
				return newTree(levels), func() {}
			}
			dir, err := os.MkdirTemp("", "arbobench")
			if err != nil {
				panic(err)
			}
			t, database := pebbleTree(dir)
			return t, func() {
				// Close before removing: a live compaction would race the delete.
				if err := database.Close(); err != nil {
					panic(err)
				}
				os.RemoveAll(dir)
			}
		}

		// add loop, 10k
		{
			keys, vals := mkKVs(10000)
			t, cleanup := mk()
			start := time.Now()
			for i := range keys {
				if err := t.Add(keys[i], vals[i]); err != nil {
					panic(err)
				}
			}
			fmt.Printf("go %-6s add loop 10k:      %v\n", backend, time.Since(start))
			cleanup()
		}

		// add_batch 10k / 100k / 1M
		for _, n := range []int{10000, 100000, 1000000} {
			keys, vals := mkKVs(n)
			t, cleanup := mk()
			start := time.Now()
			inv, err := t.AddBatch(keys, vals)
			if err != nil {
				panic(err)
			}
			fmt.Printf("go %-6s add_batch %7d: %v (invalid %d)\n", backend, n, time.Since(start), len(inv))
			cleanup()
		}

		// add_batch: 1k new keys into a populated 1M-leaf tree.
		// Go arbo's addBatchInDisk (>65536 leafs) only supports pebble;
		// on memdb it panics with "unsupported WriteTx type".
		if backend == "pebble" {
			keys, vals := mkKVs(1000000)
			t, cleanup := mk()
			if _, err := t.AddBatch(keys, vals); err != nil {
				panic(err)
			}
			keys2, vals2 := mkKVs(1000)
			start := time.Now()
			inv, err := t.AddBatch(keys2, vals2)
			if err != nil {
				panic(err)
			}
			fmt.Printf("go %-6s add_batch 1k into 1M: %v (invalid %d)\n", backend, time.Since(start), len(inv))
			cleanup()
		}

		// gen_proof: 10k proofs over a 100k tree
		{
			keys, vals := mkKVs(100000)
			t, cleanup := mk()
			if _, err := t.AddBatch(keys, vals); err != nil {
				panic(err)
			}
			start := time.Now()
			for i := 0; i < 10000; i++ {
				k := keys[rng.Intn(len(keys))]
				if _, _, _, _, err := t.GenProof(k); err != nil {
					panic(err)
				}
			}
			fmt.Printf("go %-6s gen_proof 10k/100k: %v\n", backend, time.Since(start))
			cleanup()
		}
	}
}

func main() {
	bench := flag.Bool("bench", false, "run benchmark workloads instead of generating vectors")
	out := flag.String("out", "..", "output directory for vector files")
	flag.Parse()
	if *bench {
		runBench()
		return
	}
	genVectors(*out)
	genBatchDiskVector(*out)
}
