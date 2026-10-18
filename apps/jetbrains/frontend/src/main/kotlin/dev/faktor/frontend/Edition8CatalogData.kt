// GENERATED from docs/specs/edition8/registry.json (72 Rev8 screens).
// Chunked because a single JVM UTF-8 constant is limited to 65535 bytes.
package dev.faktor.frontend

internal object Edition8CatalogData {
    private val CHUNKS: List<String> = listOf(
        """
S34	First open	Setup and start	Start Faktor	P-01 Q-11 O-04 O-05	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	23
S35	Service executable trust	Setup and start	Confirm Path	O-04 O-05	Dark Modern modal message · native.	Light DIALOGWRAPPER.	26
S57	Shell isolation setup	Setup and start	Keep Fail-Closed · JetBrains: OK / Apply	Q-10 Q-11	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	28
S36	Ready conversation	Setup and start	Run Task (composer)	M-02 A-01 Q-01 B-01 B-02 V-07 V-10	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	30
S33	Task options	Setup and start	Use Options	B-01 B-02 V-07 V-10	Dark Modern editor tab · webview.	Light DIALOGWRAPPER.	32
S37	Attachment admission	Setup and start	Use Attachments	C-03 C-05 C-06	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	34
S38	Task admission pending	Setup and start	Queue Message (composer)	V-07 D-05	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	36
S39	Ambiguous task start	Setup and start	Check Task Status	B-12 D-05	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	38
S01	Work	Work	Queue Message (composer)	M-01 M-02 M-04 G-01 A-01 C-01 Q-01 B-01 B-02 V-07 V-10 D-05 D-07	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	41
S14	Approve an action	Work	Allow Once	Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	43
S40	Answer a task question	Work	Send Answer	V-07 D-07	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	45
S22	Live activity	Work	Open Approval	M-11 G-14 Q-06 T-09 B-04 D-05 D-07 D-08 D-12 D-13	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	47
S59	Queued messages	Work	Save Edit	D-05	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	50
S70	Scoped stop actions	Work	Stop Run	A-10 X-05 D-07	Dark Modern modal message · native.	Light DIALOGWRAPPER.	52
S41	Inspect overview	Inspect and proof	Review Missing Evidence	V-07 V-08 V-09 V-10	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	56
S42	Plan and dependencies	Inspect and proof	Open AC-03	V-07 V-08	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	58
S24	Review changes	Inspect and proof	Open Diff	A-11 V-01 V-02 V-03 V-07 V-09 V-11 V-12	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	60
S43	Native diff review	Inspect and proof	Open Related Criterion	V-01 V-03	Dark Modern diff editor · VS Code diff.	Light diff viewer · DiffManager.	62
S45	Required verification	Inspect and proof	Open AC-03 Details	V-07 V-08 V-09	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	64
S46	Evidence detail	Inspect and proof	Open Full Output	V-07 V-08	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	66
S47	Proof identity mismatch	Inspect and proof	Verify Current Candidate	V-09 V-12	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	68
S44	Integration conflict	Inspect and proof	Review Conflict	V-01 V-12	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	70
S48	Finish actions	Inspect and proof	Configure Remote…	V-10 V-11	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	72
S32	Task outcome	Inspect and proof	Open Required Verification	B-04 V-07 V-08 V-09 V-10 V-11 V-12 O-06	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	74
S65	Verified outcome	Inspect and proof	Review Integrated Change	V-09 V-10	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	76
S25	Restore & fork	Inspect and proof	Review Restore…	Q-09 V-04 V-05 V-06 V-12 D-06	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	78
S49	Cancelled run	Inspect and proof	Review Kept Work	B-04 V-04 D-07	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	80
S23	Delegated agents	Agents	Send Steer	M-11 A-06 A-07 A-08 A-09 A-10 A-11 A-12 Q-06 B-04	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	83
S50	Agent detail and steer	Agents	Send Steer	A-09 A-10 A-11 A-12	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	85
S51	Compare approaches	Agents	Review Approach B	M-14 A-12 V-07	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	87
S52	Coordination	Agents	Post Note	A-12 D-07	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	89
S26	Conversation history	History	Open Conversation	V-09 D-01 D-02 D-03 D-04 D-06 D-13 D-15	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	92
S61	Empty conversation history	History	Start a Task	D-01 D-02	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	94
S71	Find in conversation	History	Go to Message	D-03	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	97
S72	Switch conversation safely	History	Switch	X-13 D-01 D-15	Dark Modern modal message · native.	Light DIALOGWRAPPER.	99
S53	Recorded outcome cohort	Usage	Open Runs	M-13 K-12 O-06	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	102
S54	Usage and quota	Usage	None (read-only)	O-06 O-07	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	104
S27	Usage & outcomes	Usage	Inspect Runs	M-13 K-12 B-04 V-09 D-07 O-06	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	106
S02	Choose a model	Models and providers	Enter selects	M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14	Dark Modern Quick Pick · native.	Light popup chooser · native.	109
S03	Provider connection	Models and providers	Save Connection · JetBrains: OK / Apply	P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	111
S04	Local inference	Models and providers	Save · JetBrains: OK / Apply	M-09 P-07 P-08 P-09 P-13 P-15 C-10	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	113
S05	Model tuning	Models and providers	Apply Tuning · JetBrains: OK / Apply	G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	115
S63	Compare exact models	Models and providers	Start Comparison	M-14 B-01 B-02	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	117
S06	Choose an agent	Models and providers	Enter selects	A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12	Dark Modern Quick Pick · native.	Light popup chooser · native.	119
S07	Custom agent	Models and providers	Save Agent · JetBrains: OK / Apply	M-05 A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	121
S08	Context	Context	Open Sources	C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	124
S62	Unavailable context usage	Context	Open Sources	C-01 C-02	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	126
S09	Add context	Context	Enter selects · Add 3 Items	C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11	Dark Modern Quick Pick · native.	Light popup chooser · native.	128
S10	Compact context	Context	Compact	C-12 K-01 K-02 K-03 K-04 I-11 V-07	Dark Modern modal message · native.	Light DIALOGWRAPPER.	130
S11	Instructions & rules	Instructions and memory	Add Rule… · JetBrains: OK / Apply	I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	134
S69	Instruction precedence	Instructions and memory	Open Project Instructions · JetBrains: OK / Apply	I-04 I-08 Q-12	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	136
S12	Memory	Instructions and memory	Save Memory · JetBrains: OK / Apply	K-05 K-10 K-11 I-11 O-09	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	138
S66	Memory deletion	Instructions and memory	Delete	K-10 K-11	Dark Modern modal message · native.	Light DIALOGWRAPPER.	140
S13	Permissions	Permissions and tools	Apply Policy · JetBrains: OK / Apply	Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	143
S15	Tools & MCP	Permissions and tools	Add Server… · JetBrains: OK / Apply	T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	145
S16	Add MCP server	Permissions and tools	Save and Start	T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15	Dark Modern editor tab · webview.	Light DIALOGWRAPPER.	147
S68	Future MCP authentication	Permissions and tools	Open Sign-In Page	T-04 T-06	Dark Modern modal message · native.	Light DIALOGWRAPPER.	149
S17	Skills & commands	Permissions and tools	Install Skill… · JetBrains: OK / Apply	I-06 S-01 S-02 S-03 S-04 S-05 S-06 S-14	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	151
S18	Lifecycle hook	Permissions and tools	Save Hook · JetBrains: OK / Apply	S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	153
S67	Hook test result	Permissions and tools	Back to Hook	S-12 S-13	Dark Modern modal message · native.	Light DIALOGWRAPPER.	155
S19	Terminal	Permissions and tools	Open Full Output	X-01 X-02 X-03 X-04 X-05 X-12 B-10 D-07	Dark Modern panel terminal · native.	Light tool window · console.	157
S20	Browser	Permissions and tools	Capture Screenshot	C-03 X-06 X-07 X-08 X-09 X-10 X-11 X-13	Dark Modern simple browser · native.	Light JCEF preview window.	159
S21	Budgets & retries	Settings and diagnostics	Apply Defaults · JetBrains: OK / Apply	B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	162
S28	Efficiency	Settings and diagnostics	Apply · JetBrains: OK / Apply	M-04 M-06 M-10 C-09 C-10 C-11 K-05 K-06 K-07 K-08 K-09 K-12	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	164
S29	General settings	Settings and diagnostics	VS Code: Settings saves valid values · JetBrains: OK / Apply	D-09 D-10 D-11 D-14 O-01 O-04 O-07 O-08 O-10 O-12 O-13	Dark Modern settings editor · native.	Light settings dialog · Configurable.	166
S64	Settings search and provenance	Settings and diagnostics	VS Code: Settings saves valid values · JetBrains: OK / Apply	D-11 O-01	Dark Modern settings editor · native.	Light settings dialog · Configurable.	168
S60	Settings revision conflict	Settings and diagnostics	Save Resolved Settings · JetBrains: OK / Apply	M-12 O-01	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	170
S55	Optional control-plane account	Settings and diagnostics	Connect Account · JetBrains: OK / Apply	P-04 O-07	Dark Modern editor tab · webview SETTINGS.	Light settings dialog · Configurable.	172
S30	Diagnostics	Settings and diagnostics	Preview Export…	B-11 B-12 D-13 O-03 O-04 O-05 O-07 O-09 O-13	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	174
S56	Protocol-blocked recovery	Settings and diagnostics	Restart Service…	D-13 O-04	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	176
S31	Workspace binding	Settings and diagnostics	Review New Setup	P-13 A-11 X-13 D-01 D-15 O-02 O-07 O-11	Dark Modern sidebar view · webview.	Light tool window · Swing/Kotlin UI DSL.	178
S58	Narrow Work panel	Settings and diagnostics	Queue Message (composer)	D-08 D-09 D-14	Dark Modern sidebar view at 320 px.	Light tool window at 320 px.	180
""",
    )

    val RECORDS: List<String> = CHUNKS
}
