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
  use ExUnit.Case, async: true

  alias AgenticKbMcp.McpServer
  alias AgenticKbMcp.RenderFixture

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

  describe "tool schema closure (B1)" do
    test "every tool schema sets additionalProperties: false" do
      for tool <- McpServer.tools() do
        assert tool["inputSchema"]["additionalProperties"] == false,
               "#{tool["name"]} inputSchema must set additionalProperties: false"
      end
    end

    test "every tool in the registry has a validated argument allow-list" do
      registered = McpServer.tools() |> Enum.map(& &1["name"]) |> Enum.sort()
      assert registered == Enum.sort(Map.keys(@deployed_pin_args))
    end
  end

  describe "validate_tool_args/2 (B1)" do
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

    test "an unknown tool is not reported as an argument error" do
      assert :ok == McpServer.validate_tool_args("kb_nonexistent", %{"whatever" => 1})
    end

    test "empty arguments are accepted for every tool" do
      for tool <- McpServer.tools() do
        assert :ok == McpServer.validate_tool_args(tool["name"], %{})
      end
    end
  end
end
