defmodule AgenticKbMcp.RenderFixture do
  def entry(overrides \\ %{}) do
    Map.merge(
      %{
        "id" => "ent-001",
        "path" => "elixir/mcp/render-contract",
        "summary" => "Renderer contract fixture",
        "content" => "Body line 1\nBody line 2",
        "score" => 0.98765,
        "confidence" => 0.83,
        "audit_n" => 7,
        "tags" => ["elixir", "mcp"],
        "source" => "kb_search",
        "score_kind" => "hybrid",
        "origin_repo" => "/tmp/peer-repo",
        "evidence" => [
          %{
            "id" => "ev-1",
            "kind" => "code",
            "citation_path" => "lib/foo.ex:10-14",
            "citation_sha" => "sha-1",
            "citation_hash" => "hash-1",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>alpha<<END>>",
            "status" => "verified",
            "verified" => true
          },
          %{
            "id" => "ev-2",
            "kind" => "doc",
            "citation_path" => "README.md:2-4",
            "citation_sha" => "sha-2",
            "citation_hash" => "hash-2",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>beta<<END>>",
            "status" => "unverified",
            "verified" => false
          },
          %{
            "id" => "ev-3",
            "kind" => "note",
            "citation_path" => "notes.md:7-9",
            "citation_sha" => "sha-3",
            "citation_hash" => "hash-3",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>gamma<<END>>",
            "status" => "deferred",
            "verified" => nil
          },
          %{
            "id" => "ev-4",
            "kind" => "note",
            "citation_path" => "notes.md:10-12",
            "citation_sha" => "sha-4",
            "citation_hash" => "hash-4",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>delta<<END>>",
            "status" => "deferred",
            "verified" => nil
          }
        ]
      },
      overrides
    )
  end

  def long_entry(id, content_size) do
    entry(%{
      "id" => id,
      "path" => "elixir/mcp/#{id}",
      "summary" => "Summary #{id}",
      "content" => String.duplicate("x", content_size),
      "evidence" => []
    })
  end

  # Multi-paragraph content: two paragraphs separated by a blank line, so
  # truncation-to-first-paragraph is actually exercised.
  def multi_paragraph_entry(overrides \\ %{}) do
    entry(
      Map.merge(
        %{
          "id" => "ent-multi",
          "content" =>
            "First paragraph line 1\nFirst paragraph line 2\n\nSecond paragraph — should be withheld.",
          "evidence" => []
        },
        overrides
      )
    )
  end

  def meta(overrides \\ %{}) do
    Map.merge(
      %{
        "index_age" => 42,
        "db_rebuilt_at" => "2026-08-15T00:00:00Z",
        "events_head_at" => "2026-08-15T00:00:00Z",
        "stale_warning" => false
      },
      overrides
    )
  end

  # Mirrors the Rust kb_get wire shape (src/commands/mcp.rs handle_kb_get /
  # full_evidence_to_json) — distinct from the compact search evidence shape.
  def full_entry(overrides \\ %{}) do
    Map.merge(
      %{
        "id" => "ent-001",
        "path" => "elixir/mcp/render-contract",
        "summary" => "Renderer contract fixture",
        "content" =>
          "Full body paragraph 1.\n\nFull body paragraph 2 — must NOT be truncated by kb_get.",
        "tags" => ["elixir", "mcp"],
        "version_ref" => "abc123",
        "is_stale" => false,
        "permanent" => true,
        "created_at" => "2026-08-01T00:00:00Z",
        "updated_at" => "2026-08-10T00:00:00Z",
        "kind" => "belief",
        "evidence_status" => "ok",
        "evidence" => [
          %{
            "id" => "ev-1",
            "entry_id" => "ent-001",
            "kind" => "code",
            "citation_path" => "lib/foo.ex:10-14",
            "citation_sha" => "sha-1",
            "citation_hash" => "hash-1",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>alpha full<<END>>",
            "derived_from" => nil,
            "recorded_at" => "2026-08-01T00:00:00Z"
          }
        ]
      },
      overrides
    )
  end
end

defmodule AgenticKbMcpTest do
  use ExUnit.Case, async: false

  alias AgenticKbMcp.McpServer
  alias AgenticKbMcp.RenderFixture

  describe "DbDiscovery layout precedence" do
    test "selects canonical, legacy, then canonical when both exist" do
      Enum.each(
        [
          {true, false, ".state/agent-kb/agent-kb.db"},
          {false, true, "agent-kb/agent-kb.db"},
          {true, true, ".state/agent-kb/agent-kb.db"}
        ],
        fn {canonical?, legacy?, expected_relative} ->
          root =
            Path.join(System.tmp_dir!(), "kb-db-discovery-#{System.unique_integer([:positive])}")

          on_exit(fn -> File.rm_rf!(root) end)
          canonical = Path.join([root, ".state", "agent-kb", "agent-kb.db"])
          legacy = Path.join([root, "agent-kb", "agent-kb.db"])

          if canonical? do
            File.mkdir_p!(Path.dirname(canonical))
            File.write!(canonical, "")
          end

          if legacy? do
            File.mkdir_p!(Path.dirname(legacy))
            File.write!(legacy, "")
          end

          assert {:ok, Path.join(root, expected_relative)} ==
                   AgenticKbMcp.DbDiscovery.discover(root)
        end
      )
    end

    test "a bare .state marker wins over a legacy db mid-migration" do
      root = Path.join(System.tmp_dir!(), "kb-db-discovery-#{System.unique_integer([:positive])}")
      on_exit(fn -> File.rm_rf!(root) end)

      # `.state/` exists (the canonical marker) but nothing has been written
      # to the canonical db yet; a legacy db already exists at this root.
      File.mkdir_p!(Path.join(root, ".state"))
      legacy = Path.join([root, "agent-kb", "agent-kb.db"])
      File.mkdir_p!(Path.dirname(legacy))
      File.write!(legacy, "")

      assert {:ok, Path.join([root, ".state", "agent-kb", "agent-kb.db"])} ==
               AgenticKbMcp.DbDiscovery.discover(root)
    end

    test "a nested .state marker stops the walk before an outer ancestor's real db" do
      outer =
        Path.join(
          System.tmp_dir!(),
          "kb-db-discovery-outer-#{System.unique_integer([:positive])}"
        )

      on_exit(fn -> File.rm_rf!(outer) end)

      outer_db = Path.join([outer, ".state", "agent-kb", "agent-kb.db"])
      File.mkdir_p!(Path.dirname(outer_db))
      File.write!(outer_db, "")

      # A nested directory has its own bare `.state/` marker (e.g. an
      # in-progress `kb init`) but no db file of its own yet. Discovery must
      # stop here — matching Rust Paths::discover — rather than continue up
      # to the outer repo's real db, or the MCP port (this module) and the
      # CLI (Rust) would silently serve two different stores for one nested
      # checkout. See the matching Rust test in src/config.rs.
      nested = Path.join(outer, "nested")
      File.mkdir_p!(Path.join(nested, ".state"))

      assert {:ok, Path.join([nested, ".state", "agent-kb", "agent-kb.db"])} ==
               AgenticKbMcp.DbDiscovery.discover(nested)
    end
  end

  @documented_omissions MapSet.new([
                          "origin_repo",
                          "score_kind",
                          "source",
                          "tags",
                          "evidence.id",
                          "evidence.citation_excerpt",
                          "evidence.citation_hash",
                          "evidence.citation_sha",
                          # superseded by evidence.status, which carries the
                          # same (and more) information; no longer rendered
                          # verbatim as "verified=".
                          "evidence.verified"
                        ])

  describe "format_entries/1 (compact search rendering)" do
    test "known entry renders [kb#id] marker, confidence and compact evidence lines" do
      output = McpServer.format_entries([RenderFixture.entry()])

      assert output =~ "## elixir/mcp/render-contract (score: 0.988)"
      assert output =~ "[kb#ent-001]"
      assert output =~ "confidence: 0.83  audit_n: 7"
      assert output =~ "evidence:"
      assert output =~ "- kind=code  citation_path=lib/foo.ex:10-14  status=verified"
      assert output =~ "- kind=doc  citation_path=README.md:2-4  status=BROKEN"
      assert output =~ "- kind=note  citation_path=notes.md:7-9  status=deferred"
      assert output =~ "full entry: kb_get"
      refute output =~ "notes.md:10-12"
      refute output =~ "<<UNTRUSTED_EXCERPT>>"
      refute output =~ "sha-1"
      refute output =~ "hash-1"
    end

    test "all four status values render distinctly" do
      evidence = [
        %{
          "kind" => "code",
          "citation_path" => "a.rs",
          "status" => "verified",
          "verified" => true
        },
        %{
          "kind" => "code",
          "citation_path" => "b.rs",
          "status" => "relocated",
          "verified" => nil
        },
        %{
          "kind" => "code",
          "citation_path" => "c.rs",
          "status" => "unverified",
          "verified" => false
        },
        %{
          "kind" => "code",
          "citation_path" => "d.rs",
          "status" => "unverified",
          "verified" => nil
        }
      ]

      output = McpServer.format_entries([RenderFixture.entry(%{"evidence" => evidence})])

      assert output =~ "- kind=code  citation_path=a.rs  status=verified"
      assert output =~ "- kind=code  citation_path=b.rs  status=relocated"
      # distinguishable hash mismatch: BROKEN emphasis
      assert output =~ "- kind=code  citation_path=c.rs  status=BROKEN"
      # ambiguous unverified (e.g. non-unique relocation match): rendered verbatim, not BROKEN
      assert output =~ "- kind=code  citation_path=d.rs  status=unverified"
    end

    test "empty evidence renders no stanza" do
      output =
        RenderFixture.entry(%{"id" => "ent-empty", "evidence" => []})
        |> then(&McpServer.format_entries([&1]))

      assert output =~ "[kb#ent-empty]"
      refute output =~ "evidence:"
    end

    test "bytes ceiling truncates at whole-entry boundary and appends omitted count" do
      entries = [
        RenderFixture.long_entry("ent-a", 14_000),
        RenderFixture.long_entry("ent-b", 14_000),
        RenderFixture.long_entry("ent-c", 14_000)
      ]

      output = McpServer.format_entries(entries)

      assert output =~ "[kb#ent-a]"
      assert output =~ "[kb#ent-b]"
      refute output =~ "[kb#ent-c]"
      assert output =~ "…(1 more entries omitted)"
      assert byte_size(output) <= 32_000
    end

    test "multi-paragraph content renders only the first paragraph plus marker and hint" do
      output = McpServer.format_entries([RenderFixture.multi_paragraph_entry()])

      assert output =~ "First paragraph line 1"
      assert output =~ "First paragraph line 2"
      assert output =~ "[kb#ent-multi]"
      assert output =~ "full entry: kb_get"
      refute output =~ "Second paragraph"
    end

    test "fixture field set is fully rendered or explicitly documented as omitted" do
      fixture = RenderFixture.entry()
      output = McpServer.format_entries([fixture])

      rendered_fields =
        [
          {"id", "[kb##{fixture["id"]}]"},
          {"path", fixture["path"]},
          {"summary", fixture["summary"]},
          {"content", "Body line 1"},
          {"score", "(score: 0.988)"},
          {"confidence", "confidence:"},
          {"audit_n", "audit_n:"},
          {"evidence", "evidence:"},
          {"evidence.kind", "kind="},
          {"evidence.citation_path", "citation_path="},
          {"evidence.status", "status="}
        ]
        |> Enum.filter(fn {_field, marker} -> output =~ marker end)
        |> Enum.map(&elem(&1, 0))
        |> MapSet.new()

      fixture_fields =
        Map.keys(fixture)
        |> MapSet.new()
        |> MapSet.union(
          fixture["evidence"]
          |> Enum.flat_map(&Map.keys/1)
          |> Enum.map(&"evidence.#{&1}")
          |> MapSet.new()
        )

      uncovered_fields =
        fixture_fields
        |> MapSet.difference(rendered_fields)
        |> MapSet.difference(@documented_omissions)

      assert MapSet.size(uncovered_fields) == 0,
             "uncovered fields: #{uncovered_fields |> MapSet.to_list() |> Enum.sort() |> Enum.join(", ")}"
    end
  end

  describe "format_entries/2 (_meta header)" do
    test "renders index age header when meta is present" do
      output = McpServer.format_entries([RenderFixture.entry()], RenderFixture.meta())

      assert output =~ "index age: 42s"
      refute output =~ "STALE WARNING"
    end

    test "stale_warning=true adds the warning line" do
      output =
        McpServer.format_entries(
          [RenderFixture.entry()],
          RenderFixture.meta(%{"stale_warning" => true})
        )

      assert output =~ "index age: 42s"

      assert output =~
               "STALE WARNING: one or more cited files changed after this entry was indexed"
    end

    test "stale_warning=false does not add the warning line" do
      output =
        McpServer.format_entries(
          [RenderFixture.entry()],
          RenderFixture.meta(%{"stale_warning" => false})
        )

      refute output =~ "STALE WARNING"
    end

    test "meta with nil index_age renders unknown rather than crashing" do
      output =
        McpServer.format_entries(
          [RenderFixture.entry()],
          RenderFixture.meta(%{"index_age" => nil})
        )

      assert output =~ "index age: unknown"
    end

    test "no meta (nil / arity-1 call) renders no header" do
      output = McpServer.format_entries([RenderFixture.entry()])
      refute output =~ "index age:"
    end
  end

  describe "render_result/1 (port envelope -> tool content)" do
    test "captures and renders _meta from a search result envelope" do
      resp = %{
        "type" => "result",
        "entries" => [RenderFixture.entry()],
        "_meta" => RenderFixture.meta(%{"stale_warning" => true})
      }

      %{"content" => [%{"type" => "text", "text" => text}]} = McpServer.render_result(resp)

      assert text =~ "index age: 42s"
      assert text =~ "STALE WARNING"
      assert text =~ "[kb#ent-001]"
    end

    test "a result map WITHOUT _meta still renders (backward compat with older Rust binary)" do
      resp = %{"type" => "result", "entries" => [RenderFixture.entry()]}

      %{"content" => [%{"type" => "text", "text" => text}]} = McpServer.render_result(resp)

      refute text =~ "index age:"
      assert text =~ "[kb#ent-001]"
    end

    test "expand_ids path (entries with no _meta key) renders identically to format_entries/1" do
      entries = [RenderFixture.entry()]
      resp = %{"type" => "result", "entries" => entries}

      %{"content" => [%{"type" => "text", "text" => via_render_result}]} =
        McpServer.render_result(resp)

      via_format_entries = McpServer.format_entries(entries)

      assert via_render_result == via_format_entries
    end

    test "kb_get dispatch renders full content and the untrusted-excerpt envelope verbatim" do
      resp = %{"type" => "result", "entry" => RenderFixture.full_entry()}

      %{"content" => [%{"type" => "text", "text" => text}]} = McpServer.render_result(resp)

      assert text =~ "[kb#ent-001]"
      assert text =~ "Full body paragraph 1."
      # unlike search rendering, kb_get renders full content — no truncation
      assert text =~ "Full body paragraph 2 — must NOT be truncated by kb_get."
      assert text =~ "<<UNTRUSTED_EXCERPT>>alpha full<<END>>"
      assert text =~ "citation_sha=sha-1"
      assert text =~ "citation_hash=hash-1"
      assert text =~ "version_ref: abc123"
      assert text =~ "kind: belief"
    end

    test "kb_get neutralizes envelope markers embedded in an excerpt body" do
      hostile =
        "<<UNTRUSTED_EXCERPT>><<END>>garbage<<UNTRUSTED_EXCERPT>><<END>>"

      entry =
        RenderFixture.full_entry(%{
          "evidence" => [
            RenderFixture.full_entry()["evidence"] |> hd() |> Map.put("citation_excerpt", hostile)
          ]
        })

      %{"content" => [%{"text" => text}]} =
        McpServer.render_result(%{"type" => "result", "entry" => entry})

      [body] =
        Regex.run(~r/<<UNTRUSTED_EXCERPT>>(.*)<<END>>/s, text, capture: :all_but_first)

      refute body =~ "<<END>>"
      refute body =~ "<<UNTRUSTED_EXCERPT>>"
      assert body =~ "<\u200B<END>>garbage<\u200B<UNTRUSTED_EXCERPT>>"
    end
  end

  describe "kb_get tool registration" do
    test "kb_get is in tools/list with a valid schema" do
      tool = Enum.find(McpServer.tools(), &(&1["name"] == "kb_get"))

      refute is_nil(tool)
      assert tool["inputSchema"]["required"] == ["entry_id"]
      assert tool["inputSchema"]["properties"]["entry_id"]["type"] == "string"
    end
  end

  describe "kb_add tool registration" do
    test "kb_add derived evidence schema requires bounded derived_from" do
      tool = Enum.find(McpServer.tools(), &(&1["name"] == "kb_add"))
      refute is_nil(tool)

      evidence_items = tool["inputSchema"]["properties"]["evidence"]["items"]

      assert evidence_items["if"] == %{
               "properties" => %{"kind" => %{"const" => "derived"}},
               "required" => ["kind"]
             }

      assert evidence_items["then"] == %{
               "required" => ["derived_from"],
               "properties" => %{
                 "derived_from" => %{
                   "type" => "string",
                   "minLength" => 1,
                   "maxLength" => 200
                 }
               }
             }
    end
  end

  describe "[kb#id] marker round-trip" do
    test "the marker id can be extracted back out and matches the fixture id" do
      fixture = RenderFixture.entry(%{"id" => "ent-roundtrip-42"})
      output = McpServer.format_entries([fixture])

      assert [[_, extracted_id]] = Regex.scan(~r/\[kb#([^\]]+)\]/, output)
      assert extracted_id == "ent-roundtrip-42"
    end
  end

  # ── B1 (ADR-4): reject unknown arguments at the outermost layer ────────────
  #
  # Every request field the DEPLOYED machines_conf pin sends is enumerated in
  # docs/decisions/b1-request-contract.md. The pin is agentic-kb rev
  # 058f82bdb650a1de44de167adea0672c54f1f2c1 (machines_conf flake.lock) whose
  # `dispatch_tool/3` clauses are byte-identical to this branch's.
  @deployed_pin_args %{
    "kb_search" => [
      "query",
      "limit",
      "mode",
      "path_prefix",
      "tag",
      "inline_verify_k",
      "expand_ids"
    ],
    "kb_add" => [
      "path",
      "summary",
      "content",
      "tags",
      "permanent",
      "replace_path",
      "kind",
      "evidence",
      "cues"
    ],
    "kb_cite" => ["path", "start", "end"],
    "kb_import" => ["path", "upsert"],
    "kb_stale_check" => ["files", "commits", "blame"],
    "kb_expire" => ["entry_id", "reason", "force"],
    "kb_run" => ["test_id", "result", "adapter", "detail"],
    "kb_test_add" => ["app", "name", "protocol", "config", "test_id"],
    "kb_tests" => ["app"],
    "kb_reembed" => ["dry_run", "max_chars"],
    "kb_compact" => [],
    "kb_rebuild" => [],
    "kb_get" => ["entry_id"]
  }

  @s1_tool_args %{
    "kb_audit_run" => ["sample_size", "mode"],
    "kb_audit_record" => ["run_id", "verdicts"],
    "kb_audit_report" => [],
    "kb_provenance" => ["entry_id", "max_depth"]
  }

  # Successful wire envelopes emitted by every Rust handler reachable through
  # dispatch_tool/3. Errors all share the final {type:error,message} renderer.
  # Reembed is intentionally represented by each of its three return sites.
  @response_shapes %{
    "search" => [
      %{"type" => "result", "entries" => []},
      %{"type" => "result", "entries" => [], "_meta" => RenderFixture.meta()}
    ],
    "add" => [
      %{"type" => "ok", "entry_id" => "new-1"},
      %{"type" => "ok", "entry_id" => "new-1", "similar_existing" => [%{"id" => "old-1", "path" => "p", "summary" => "s", "score" => 0.91}]}
    ],
    "cite" => [%{"type" => "result", "citation_path" => "a.ex", "citation_sha" => nil, "citation_hash" => "h", "file_size" => 12}],
    "import" => [%{"type" => "ok", "imported" => 1, "skipped" => 0}],
    "stale_check" => [%{"type" => "result", "stale" => [], "review" => [], "unreachable" => [], "checked" => 0}],
    "expire" => [%{"type" => "ok", "expired" => "entry-1"}],
    "run" => [%{"type" => "ok", "run_id" => "run-1", "test_id" => "test-1", "result" => "pass"}],
    "test_add" => [%{"type" => "ok", "test_id" => "test-1"}],
    "tests" => [%{"type" => "result", "test_cases" => [], "count" => 0}],
    "reembed" => [
      %{"type" => "ok", "embedded" => 0, "failed" => 0, "failures" => [], "skipped" => 0,
        "missing" => 1, "raced" => 0, "dry_run" => false, "noop_embedder" => true,
        "message" => "KB_NO_EMBED is set — no embedder available"},
      %{"type" => "ok", "embedded" => 0, "failed" => 0, "failures" => [], "skipped" => 1,
        "missing" => 2, "raced" => 0, "dry_run" => true, "noop_embedder" => false},
      %{"type" => "ok", "embedded" => 2, "failed" => 1,
        "failures" => [%{"id" => "bad", "cause" => "boom"}], "skipped" => 1, "missing" => 4,
        "raced" => 1, "dry_run" => false, "noop_embedder" => false}
    ],
    "compact" => [%{"type" => "ok", "before" => 4, "after" => 2}],
    "rebuild" => [%{"type" => "ok", "rebuilt" => 3, "truncated_tail" => nil}],
    "kb_get" => [%{"type" => "result", "entry" => RenderFixture.full_entry()}],
    "audit_run" => [
      %{"type" => "ok", "run_id" => "audit-1", "samples" => []},
      %{
        "type" => "ok",
        "run_id" => "audit-2",
        "samples" => [
          %{
            "id" => "e1",
            "path" => "p/e1",
            "summary" => "s1",
            "kind" => "belief",
            "evidence_status" => "present",
            "arm" => "uniform",
            "evidence" => []
          }
        ]
      }
    ],
    "audit_record" => [%{"type" => "ok", "recorded" => 1, "expired" => 0}],
    "audit_report" => [
      %{"type" => "result", "per_kind_session_precision" => [], "last_run_at" => nil, "total_runs" => 0, "per_arm_precision" => []},
      %{"type" => "result", "per_kind_session_precision" => [], "last_run_at" => "2026-09-05T00:00:00Z", "total_runs" => 1, "per_arm_precision" => [], "injection_telemetry" => %{"eligible" => 1}}
    ],
    "provenance" => [%{"type" => "result", "roots" => ["root-1"], "graph" => [], "truncated" => false}]
  }

  # IMPORTANT (premium review of bd-21ef.2..bd-21ef.2.12b): the shape-table
  # test below only refuted the raw-JSON fallback, but mcp_server.ex has a
  # catch-all `%{"type" => "ok"} -> "OK"` clause ahead of that fallback — so
  # deleting a specific `"ok"`-shaped renderer clause (audit_run, audit_record,
  # add, run, etc.) still rendered "OK" and the test stayed green. One
  # expected substring per shape, in the same order as @response_shapes,
  # closes that hole alongside `refute text == "OK"`.
  @response_shape_expected_substrings %{
    "search" => ["(no results)", "(no results)"],
    "add" => ["Added entry new-1", "Added entry new-1"],
    "cite" => ["citation_path=a.ex"],
    "import" => ["Imported 1 entries"],
    "stale_check" => ["Checked 0 file(s)"],
    "expire" => ["Expired entry entry-1"],
    "run" => ["Recorded run run-1"],
    "test_add" => ["Added test case test-1"],
    "tests" => ["(no test cases)"],
    "reembed" => ["KB_NO_EMBED is set", "[dry-run]", "1 failed"],
    "compact" => ["Compacted: 4 events"],
    "rebuild" => ["Rebuilt 3 entries"],
    "kb_get" => ["[kb#ent-001]"],
    "audit_run" => ["Audit run audit-1", "id=e1"],
    "audit_record" => ["Recorded 1 audit verdict"],
    "audit_report" => ["Audit report:", "Audit report:"],
    "provenance" => ["Provenance roots:"]
  }

  describe "S1 response rendering" do
    test "kb_add renders near-duplicate details" do
      response = @response_shapes["add"] |> tl() |> hd()
      %{"content" => [%{"text" => text}]} = McpServer.render_result(response)

      assert text =~ "Similar existing entries"
      assert text =~ "id=old-1"
      assert text =~ "path=p"
      assert text =~ "summary=s"
      assert text =~ "score=0.91"
    end

    # IMPORTANT (premium review of bd-21ef.2..bd-21ef.2.12b): `Map.get(resp,
    # "similar_existing", [])` only supplies its default when the key is
    # ABSENT — an explicit JSON `null` (a real possibility once similar_existing
    # is derived from a Rust `Option`) makes `Map.get` return `nil`, and
    # `Enum.map_join(nil, ...)` raises `Protocol.UndefinedError`.
    test "kb_add renders fine when similar_existing is an explicit null" do
      response = %{"type" => "ok", "entry_id" => "new-1", "similar_existing" => nil}
      %{"content" => [%{"text" => text}]} = McpServer.render_result(response)

      assert text == "Added entry new-1."
    end

    # L3 gating fix: raced (rows dropped by the exclusion-correct reembed
    # write path — see src/commands/reembed.rs ReembedReport.raced) and the
    # per-id failure causes were both present on the wire but silently
    # dropped by this renderer, leaving an operator with no way to tell
    # from the rendered text alone why embedded + failed < missing, or
    # which entry failed and how.
    test "kb_reembed renders the raced count and each failure's cause" do
      response = %{
        "type" => "ok",
        "embedded" => 2,
        "failed" => 1,
        "failures" => [%{"id" => "bad", "cause" => "fixture embedding failure"}],
        "skipped" => 1,
        "missing" => 4,
        "raced" => 1,
        "dry_run" => false,
        "noop_embedder" => false
      }

      %{"content" => [%{"text" => text}]} = McpServer.render_result(response)

      assert text =~ "1 raced"
      assert text =~ "bad"
      assert text =~ "fixture embedding failure"
    end

    test "every dispatched Rust success shape has a specific renderer" do
      for {method, shapes} <- @response_shapes,
          {shape, index} <- Enum.with_index(shapes) do
        %{"content" => [%{"type" => "text", "text" => text}]} = McpServer.render_result(shape)
        fallback = shape |> :json.encode() |> IO.iodata_to_binary()

        refute text == fallback,
               "#{method} #{inspect(Map.keys(shape))} fell through to the generic JSON renderer"

        # A deleted "ok"-shaped renderer clause falls through to the generic
        # `%{"type" => "ok"} -> "OK"` catch-all, which is neither the raw-JSON
        # fallback above nor caught by it — this refute is the actual gate.
        refute text == "OK",
               "#{method} #{inspect(Map.keys(shape))} fell through to the generic \"ok\" renderer"

        expected = Enum.at(@response_shape_expected_substrings[method], index)

        assert expected,
               "no expected substring declared for #{method} shape #{index} — add one to @response_shape_expected_substrings"

        assert text =~ expected,
               "#{method} shape #{index} did not render #{inspect(expected)}: #{inspect(text)}"
      end

      error = %{"type" => "error", "code" => "db_error", "message" => "boom"}
      assert %{"isError" => true, "content" => [%{"text" => "boom"}]} = McpServer.render_result(error)
    end
  end

  describe "tool schema closure (B1)" do
    test "every tool schema sets additionalProperties: false" do
      for tool <- McpServer.tools() do
        assert tool["inputSchema"]["additionalProperties"] == false,
               "#{tool["name"]} inputSchema must set additionalProperties: false"
      end
    end

    # Every bound B1 enforces on the Rust side must be visible in the schema,
    # so a client validates before calling instead of learning the cap from an
    # error. inline_verify_k shipped with a minimum and no maximum.
    test "every bounded numeric argument advertises both of its bounds" do
      expected = %{
        {"kb_search", "limit"} => {1, 100},
        {"kb_search", "inline_verify_k"} => {0, 100},
        {"kb_reembed", "max_chars"} => {1, 100_000}
      }

      for {{tool_name, field}, {min, max}} <- expected do
        schema =
          McpServer.tools()
          |> Enum.find(&(&1["name"] == tool_name))
          |> get_in(["inputSchema", "properties", field])

        assert schema["minimum"] == min, "#{tool_name}.#{field} minimum"
        assert schema["maximum"] == max, "#{tool_name}.#{field} maximum"

        assert schema["description"] =~ to_string(max),
               "#{tool_name}.#{field} description must name its cap"
      end
    end

    test "every tool in the registry has a validated argument allow-list" do
      registered = McpServer.tools() |> Enum.map(& &1["name"]) |> Enum.sort()
      expected = Map.keys(@deployed_pin_args) ++ Map.keys(@s1_tool_args)
      assert registered == Enum.sort(expected)
    end
  end

  describe "validate_tool_args/2 (B1)" do
    test "kb_audit_record requires notes for false verdicts only" do
      assert {:error, message} =
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [%{"entry_id" => "entry-1", "verdict" => false}]
               })

      assert message =~ "entry-1"
      assert message =~ "non-empty note"

      assert {:error, _message} =
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [
                   %{"entry_id" => "entry-1", "verdict" => false, "note" => "  \t"}
                 ]
               })

      assert :ok ==
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [
                   %{"entry_id" => "entry-1", "verdict" => false, "note" => "unsupported"},
                   %{"entry_id" => "entry-2", "verdict" => true}
                 ]
               })
    end

    # CRITICAL (premium review of bd-21ef.2..bd-21ef.2.12b): `verdict["verdict"]
    # == false` never fires for a missing or non-boolean `verdict` key, so a
    # verdict item shaped like %{"entry_id" => "x"} (no verdict key at all)
    # or %{"entry_id" => "x", "verdict" => "false"} (a string, not a boolean)
    # passed this check and reached the Rust port, which used to coerce the
    # same shape to `false` via `.unwrap_or(false)` and expire the entry with
    # no note. All three malformed shapes below (missing verdict, wrong-typed
    # verdict, missing entry_id) must be rejected here, before the Rust call.
    test "kb_audit_record rejects a verdict item with a missing or non-boolean verdict" do
      assert {:error, _} =
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [%{"entry_id" => "entry-1"}]
               })

      assert {:error, _} =
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [%{"entry_id" => "entry-1", "verdict" => "false"}]
               })

      assert {:error, _} =
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [%{"verdict" => true}]
               })

      assert :ok ==
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [%{"entry_id" => "entry-1", "verdict" => true}]
               })
    end

    test "kb_audit_record rejects an unknown key inside a verdict item" do
      assert {:error, message} =
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => [%{"entry_id" => "entry-1", "verdict" => true, "extra" => 1}]
               })

      assert message =~ "extra"
    end

    test "kb_audit_record accepts 50 verdicts and rejects 51" do
      assert :ok ==
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => List.duplicate(%{"entry_id" => "entry", "verdict" => true}, 50)
               })

      assert {:error, message} =
               McpServer.validate_tool_args("kb_audit_record", %{
                 "run_id" => "audit-1",
                 "verdicts" => List.duplicate(%{"entry_id" => "entry", "verdict" => true}, 51)
               })

      assert message =~ "50"
    end

    test "kb_audit_record schema declares conditional notes and the verdict cap" do
      schema =
        McpServer.tools()
        |> Enum.find(&(&1["name"] == "kb_audit_record"))
        |> get_in(["inputSchema", "properties", "verdicts"])

      assert schema["maxItems"] == 50
      assert schema["items"]["if"]["properties"]["verdict"] == %{"const" => false}
      assert schema["items"]["then"]["required"] == ["note"]
      assert schema["items"]["required"] == ["entry_id", "verdict"]
      assert schema["items"]["additionalProperties"] == false
    end

    # IMPORTANT (premium review of bd-21ef.2..bd-21ef.2.12b): run_id's schema
    # declared minLength/maxLength but no `pattern`, so nothing in the
    # declarative schema matched the Rust rule
    # (`run_id.bytes().any(|b| b < 0x20)`) that actually rejects control
    # characters. The pattern is documentation for MCP clients introspecting
    # the schema — the Rust binary is still the enforcement point.
    test "kb_audit_record schema's run_id pattern matches the Rust no-control-chars rule" do
      schema =
        McpServer.tools()
        |> Enum.find(&(&1["name"] == "kb_audit_record"))
        |> get_in(["inputSchema", "properties", "run_id"])

      assert %{"pattern" => pattern} = schema
      regex = Regex.compile!(pattern)
      # A space (0x20) is not a control character and the Rust rule
      # accepts it; only bytes strictly below 0x20 are rejected.
      assert Regex.match?(regex, "plain run id")
      refute Regex.match?(regex, "bad\nid")
      refute Regex.match?(regex, "bad\tid")
    end

    test "an unknown argument is rejected, not silently dropped by put_if_present" do
      assert {:error, message} =
               McpServer.validate_tool_args("kb_add", %{
                 "path" => "a/b",
                 "summary" => "s",
                 "content" => "c",
                 "confidence" => 0.9
               })

      assert message =~ "confidence"
      assert message =~ "unknown argument"
    end

    test "the rejection names every unknown key, sorted" do
      assert {:error, message} =
               McpServer.validate_tool_args("kb_search", %{
                 "query" => "q",
                 "zeta" => 1,
                 "alpha" => 2
               })

      assert message =~ "alpha"
      assert message =~ "zeta"
      assert String.contains?(message, "alpha, zeta")
    end

    test "a tool with no arguments rejects any argument" do
      assert {:error, message} = McpServer.validate_tool_args("kb_compact", %{"vacuum" => true})
      assert message =~ "vacuum"
    end

    test "every field the deployed machines_conf pin sends is accepted" do
      for {tool, fields} <- @deployed_pin_args do
        args = Map.new(fields, fn field -> {field, nil} end)

        assert :ok == McpServer.validate_tool_args(tool, args),
               "#{tool} must accept the deployed pin field set #{inspect(fields)}"
      end
    end

    test "every S1 audit and provenance field is accepted" do
      for {tool, fields} <- @s1_tool_args do
        assert :ok == McpServer.validate_tool_args(tool, Map.new(fields, &{&1, nil}))
      end
    end

    # Pins the total contract independently of the tools/call ordering, so a
    # later reordering cannot mask the nil-arguments crash.
    # Pins the total contract independently of the tools/call ordering, so a
    # later reordering cannot mask the missing-arguments crash. `:null` is what
    # OTP's `:json` yields for an explicit JSON null; `nil` is a missing key.
    test "absent arguments are accepted rather than raising FunctionClauseError" do
      assert :ok == McpServer.validate_tool_args("kb_search", nil)
      assert :ok == McpServer.validate_tool_args("kb_search", :null)
    end

    test "an unknown tool is not reported as an argument error" do
      assert :ok == McpServer.validate_tool_args("kb_nonexistent", %{"whatever" => 1})
    end

    test "empty arguments are accepted for every tool" do
      for tool <- McpServer.tools() do
        assert :ok == McpServer.validate_tool_args(tool["name"], %{})
      end
    end
  end

  # B1 finding 8: the wiring at the tools/call clause — not validate_tool_args/2
  # in isolation — is what the fleet actually exercises. These drive the real
  # request path with raw JSON bytes: handle_cast({:line, ...}) -> json_decode
  # -> handle_request -> validate -> dispatch, with db_path: nil so no port is
  # needed. Raw bytes matter: OTP's `:json` decodes JSON `null` to the atom
  # `:null`, which no round-trip through an Elixir map would reproduce.
  describe "tools/call request path (B1)" do
    # A db_path that exists as far as the server is concerned, so argument
    # normalisation and validation actually run. Requests that clear validation
    # reach PortManager with no manager started and come back as an envelope,
    # never a crash.
    @with_db %{db_path: "/nonexistent/agent-kb.db"}
    @without_db %{db_path: nil}

    defp call_line(line, state) do
      output =
        ExUnit.CaptureIO.capture_io(fn ->
          assert {:noreply, _state} = McpServer.handle_cast({:line, line}, state)
        end)

      output |> String.trim() |> :json.decode()
    end

    defp start_fake_port do
      fake = Path.expand("support/fake_port.sh", __DIR__)

      start_supervised!(
        {AgenticKbMcp.PortManager,
         db_path: "unused", kb_bin: fake, name: AgenticKbMcp.PortManager}
      )
    end

    test "kb_audit_run dispatches through the real tools/call and port paths" do
      start_fake_port()
      response = call_line(~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_audit_run","arguments":{"sample_size":5,"mode":"uniform"}}}), @with_db)
      assert get_in(response, ["result", "content", Access.at(0), "text"]) =~ "Audit run audit-1"
    end

    test "kb_audit_record dispatches through the real tools/call and port paths" do
      start_fake_port()
      response = call_line(~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_audit_record","arguments":{"run_id":"audit-1","verdicts":[]}}}), @with_db)
      assert get_in(response, ["result", "content", Access.at(0), "text"]) =~ "Recorded 1 audit verdict"
    end

    test "kb_audit_report dispatches through the real tools/call and port paths" do
      start_fake_port()
      response = call_line(~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_audit_report","arguments":{}}}), @with_db)
      assert get_in(response, ["result", "content", Access.at(0), "text"]) =~ "Audit report: 0"
    end

    test "kb_provenance dispatches through the real tools/call and port paths" do
      start_fake_port()
      response = call_line(~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_provenance","arguments":{"entry_id":"entry-1","max_depth":64}}}), @with_db)
      assert get_in(response, ["result", "content", Access.at(0), "text"]) =~ "Provenance roots: root-1"
    end

    # L3 gating fix: with KB_NO_EMBED set, run_reembed returns early with no
    # writes attempted at all — the noop_embedder flag alone does not reach
    # a human, only resp["message"] rendered into the tool's text output
    # does. This drives the real dispatch -> port -> render_result path
    # end to end, rather than calling render_result directly, so a future
    # regression in the wiring between them (not just the renderer itself)
    # is caught here too.
    test "kb_reembed dispatches through the real tools/call and port paths and surfaces the noop message" do
      start_fake_port()
      response = call_line(~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_reembed","arguments":{}}}), @with_db)
      assert get_in(response, ["result", "content", Access.at(0), "text"]) =~ "KB_NO_EMBED is set"
    end

    test "an explicit null arguments does not crash the server" do
      response =
        call_line(
          ~s({"jsonrpc":"2.0","id":1,"method":"tools/call",) <>
            ~s("params":{"name":"kb_search","arguments":null}}),
          @with_db
        )

      # The point is that normalisation ran and nothing raised: whatever the
      # port lane answers, it is a well-formed result envelope.
      refute Map.has_key?(response, "error")
      assert %{"result" => %{"content" => [%{"text" => text}]}} = response
      refute text =~ "unknown argument"
      refute text =~ "arguments must be"
    end

    test "an omitted arguments key does not crash the server" do
      response =
        call_line(
          ~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_search"}}),
          @with_db
        )

      refute Map.has_key?(response, "error")
      assert %{"result" => %{"content" => [%{"text" => text}]}} = response
      refute text =~ "unknown argument"
      refute text =~ "arguments must be"
    end

    test "a non-object arguments is rejected rather than coerced" do
      response =
        call_line(
          ~s({"jsonrpc":"2.0","id":1,"method":"tools/call",) <>
            ~s("params":{"name":"kb_search","arguments":"oops"}}),
          @with_db
        )

      assert %{"result" => %{"isError" => true, "content" => [%{"text" => text}]}} = response
      assert text =~ "arguments must be a JSON object"
    end

    test "an unknown argument is rejected through the real tools/call path" do
      response =
        call_line(
          ~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_add",) <>
            ~s("arguments":{"path":"a/b","summary":"s","content":"c","confidence":0.9}}}),
          @with_db
        )

      assert %{"result" => %{"isError" => true, "content" => [%{"text" => text}]}} = response
      assert text =~ "confidence"
      assert text =~ "unknown argument"
    end

    test "an uninitialised repo answers the kb-init hint, not an argument error" do
      response =
        call_line(
          ~s({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kb_search",) <>
            ~s("arguments":{"query":"q","bogus":1}}}),
          @without_db
        )

      assert %{"result" => %{"content" => [%{"text" => text}]}} = response

      assert text =~ "No agent-kb.db found",
             "the actionable hint must win over an argument error when there is no DB"
    end
  end
end
