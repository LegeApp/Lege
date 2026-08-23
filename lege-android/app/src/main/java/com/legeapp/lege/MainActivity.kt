package com.legeapp.lege

import android.app.ActivityManager
import android.app.AlertDialog
import android.content.Context
import android.content.Intent
import android.content.ContentValues
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.net.Uri
import android.os.Bundle
import android.os.Build
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.provider.OpenableColumns
import android.provider.DocumentsContract
import android.text.Editable
import android.text.TextWatcher
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import java.io.File
import java.util.concurrent.Executors

/** Dependency-free host styled after the supplied Modernist 1a design direction. */
class MainActivity : android.app.Activity() {
    private companion object {
        const val INPUT = 1; const val OUTPUT = 2
        var INK = "#1E1C22"; var GROUND = "#C4D1CC"; var SURFACE = "#CAD4D0"
        var FIELD = "#CCD5D2"; var MUTED = "#62686A"; var ACCENT = "#A35B46"; var LIGHT = "#EEF2F0"
    }
    private data class ThemeSpec(val name: String, val ink: String, val ground: String, val surface: String, val field: String, val muted: String, val accent: String, val light: String)
    private val themes = listOf(
        ThemeSpec("Glaucous Green · Light", "#1E1C22", "#C7DAD7", "#CDDEDB", "#CFDFDD", "#62686A", "#A35B46", "#EEF2F0"),
        ThemeSpec("Golden Yellow · Light", "#3A393D", "#FFA83F", "#FFB152", "#FFB45A", "#89653E", "#0A6CA3", "#FFF4DF"),
        ThemeSpec("Raw Sienna · Light", "#20192C", "#C76B1D", "#CD7A34", "#CF803D", "#633A26", "#493122", "#FFF0DD"),
        ThemeSpec("Glaucous Green · Dark", "#C7DAD7", "#1E1C22", "#312527", "#392929", "#879292", "#A35B46", "#1E1C22"),
        ThemeSpec("Golden Yellow · Dark", "#FFA83F", "#3A393D", "#33404B", "#304351", "#B47E3E", "#0A6CA3", "#3A393D"),
        ThemeSpec("Raw Sienna · Dark", "#C76B1D", "#20192C", "#261C2B", "#281E2A", "#884C23", "#493122", "#20192C"),
    )
    private val worker = Executors.newSingleThreadExecutor()
    private val main = Handler(Looper.getMainLooper())
    private lateinit var root: LinearLayout; private lateinit var form: ScrollView; private lateinit var formBody: LinearLayout; private lateinit var footer: LinearLayout
    private lateinit var documentTitle: TextView; private lateinit var saveMeta: TextView
    private lateinit var targetHeight: EditText; private lateinit var pageRange: EditText; private lateinit var sauvolaK: EditText; private lateinit var fixedThreshold: EditText
    private lateinit var imageHandling: SegmentedChoice; private lateinit var ocrMode: SegmentedChoice; private lateinit var marginMode: SegmentedChoice; private lateinit var binarizationMode: SegmentedChoice; private lateinit var binarizationOptions: LinearLayout
    private lateinit var highQuality: SquareToggle; private lateinit var layout: SquareToggle; private lateinit var epub: SquareToggle; private lateinit var inverted: SquareToggle
    private lateinit var jpegCompat: SquareToggle
    private var taskId: Long? = null; private var inputFile: File? = null; private var outputUri: Uri? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        applyTheme(getSharedPreferences("lege", MODE_PRIVATE).getInt("theme", 0), persist = false)
        val manager = getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        LegeNative.nativeInit(filesDir.absolutePath, cacheDir.absolutePath, manager.largeMemoryClass)
        setContentView(buildScreen())
    }
    override fun onDestroy() { taskId?.let(LegeNative::nativeCancel); worker.shutdownNow(); super.onDestroy() }

    @Deprecated("Avoids an AndroidX dependency in this intentionally small host.")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data); val uri = data?.data
        if (requestCode == INPUT && resultCode == RESULT_OK && uri != null) worker.execute {
            data?.flags?.let { flags -> runCatching { contentResolver.takePersistableUriPermission(uri, flags and (Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)) } }
            runCatching { copyInput(uri) }.onSuccess { file -> main.post {
                inputFile = file; documentTitle.text = displayName(uri)
                outputUri = savedOutputFolder()
                saveMeta.text = if (outputUri == null) "Downloads (default)" else "Last output folder"
            } }.onFailure { error -> main.post { documentTitle.text = "Could not read input: ${error.message}" } }
        }
        if (requestCode == OUTPUT && resultCode == RESULT_OK && uri != null) {
            val takeFlags = (data?.flags ?: 0) and (Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            val retained = runCatching { contentResolver.takePersistableUriPermission(uri, takeFlags) }.isSuccess
            if (retained && canWriteFolder(uri)) { outputUri = uri; rememberOutputFolder(uri); saveMeta.text = displayName(uri) } else notice("Android did not grant write access to that folder. Using Downloads instead.")
        }
    }

    private fun buildScreen(): View {
        root = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setBackgroundColor(c(GROUND)) }
        form = ScrollView(this).apply { isFillViewport = true }
        form.addView(buildForm()); root.addView(form, lp(match, 0, 1f)); return root
    }
    private fun header() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL; setPadding(dp(18), dp(14), dp(18), dp(14)); background = bg(SURFACE)
        addView(text("LEGE", 22f, INK, true, .08f))
        addView(LinearLayout(context).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(LinearLayout(context).apply { orientation = LinearLayout.VERTICAL; addView(button("SOURCE", false).apply { setOnClickListener { chooseInput() } }, lp(match, dp(42))); documentTitle = text("Select a PDF", 12f, MUTED).also { addView(it, lp(match, wrap).top(5)) } }, lp(0, wrap, 1f).margins(0, 14, 6, 0))
            addView(LinearLayout(context).apply { orientation = LinearLayout.VERTICAL; addView(button("SAVE TO", false).apply { setOnClickListener { chooseOutput() } }, lp(match, dp(42))); saveMeta = text(if (savedOutputFolder() == null) "Downloads (default)" else "Last output folder", 12f, MUTED).also { addView(it, lp(match, wrap).top(5)) } }, lp(0, wrap, 1f).margins(6, 14, 0, 0))
        }, lp(match, wrap))
        addView(rule(), lp(match, dp(2)).top(14))
    }
    private fun buildForm(): View {
        formBody = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(0, dp(8), 0, dp(20)) }
        formBody.addView(header(), lp(match, wrap))
        section("OUTPUT"); targetHeight = field("Target height", "Pixels; default 1200", savedTargetHeight()); targetHeight.addTextChangedListener(object : TextWatcher { override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) = Unit; override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) = Unit; override fun afterTextChanged(s: Editable?) { s?.toString()?.toIntOrNull()?.takeIf { it > 0 }?.let { getSharedPreferences("lege", MODE_PRIVATE).edit().putInt("target_height", it).apply() } } }); highQuality = toggle("High-quality output", null, false); jpegCompat = toggle("Compatibility (CCITT + JPEG)", "Use maximum-compatible PDF encoders", false)
        section("BINARIZATION"); formBody.addView(text("Method", 15f, INK), lp(match, wrap).margins(18, 12, 18, 4)); binarizationMode = SegmentedChoice(listOf("ADAPTIVE", "THRESHOLD", "SAUVOLA"), -1, true).also { formBody.addView(it, lp(match, wrap).margins(18, 0, 18, 0)) }; binarizationOptions = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }; formBody.addView(binarizationOptions, lp(match, wrap)); binarizationMode.onChanged = { updateBinarizationOptions() }
        section("TEXT & OCR"); formBody.addView(text("Image regions", 15f, INK), lp(match, wrap).margins(18, 12, 18, 4)); imageHandling = SegmentedChoice(listOf("ORIGINAL", "DITHERED"), 0).also { formBody.addView(it, lp(match, wrap).margins(18, 0, 18, 8)) }
        layout = toggle("Layout detection", "Needs Vulkan; falls back if unusable", true)
        formBody.addView(text("OCR text layer", 15f, INK), lp(match, wrap).margins(18, 12, 18, 4)); ocrMode = SegmentedChoice(listOf("FAST", "THOROUGH"), -1, true).also { formBody.addView(it, lp(match, wrap).margins(18, 0, 18, 8)) }
        epub = toggle("Also create EPUB", "Requires thorough OCR", false).also { control -> control.onChanged = { enabled -> if (enabled) ocrMode.select(1) } }
        ocrMode.onChanged = { selected -> if (selected < 0 && epub.checked) epub.setChecked(false) }
        section("PAGES"); pageRange = field("Page range", "e.g. 5 or 1-20, one-based", ""); inverted = toggle("Inverted source", "Light text on dark", false)
        section("ADVANCED"); formBody.addView(text("Page treatment", 15f, INK), lp(match, wrap).margins(18, 12, 18, 4)); marginMode = SegmentedChoice(listOf("CENTER", "CROP", "REFLOW"), -1, true).also { formBody.addView(it, lp(match, wrap).margins(18, 0, 18, 8)) }
        formBody.addView(text("Center keeps the page size and centers its content. Crop trims to content and resizes.\nReflow rebuilds text into a clean single-column layout for the target device.", 11.5f, MUTED), lp(match, wrap).margins(18, 14, 18, 0)); footer = buildFooter(); formBody.addView(footer, lp(match, wrap).top(18)); return formBody
    }
    private fun section(name: String) { formBody.addView(text(name, 10.5f, MUTED, true, .2f), lp(match, wrap).margins(18, 16, 18, 4)); formBody.addView(rule(), lp(match, dp(2)).margins(18, 0, 18, 0)) }
    private fun field(name: String, hint: String, value: String): EditText {
        val edit = EditText(this).apply { setText(value); textSize = 14f; typeface = Typeface.DEFAULT_BOLD; setTextColor(c(INK)); isSingleLine = true; setPadding(dp(9), 0, dp(9), 0); background = bg(FIELD, 2) }
        formBody.addView(row(name, hint, edit), lp(match, wrap)); return edit
    }
    private fun compactField(value: String) = EditText(this).apply { setText(value); textSize = 14f; typeface = Typeface.DEFAULT_BOLD; setTextColor(c(INK)); isSingleLine = true; setPadding(dp(9), 0, dp(9), 0); background = bg(FIELD, 2) }
    private fun updateBinarizationOptions() { binarizationOptions.removeAllViews(); when (binarizationMode.value) { 0 -> { sauvolaK = compactField("0.05"); binarizationOptions.addView(row("Sauvola k", "0.0–1.0; lower is darker", sauvolaK), lp(match, wrap)) }; 1 -> { fixedThreshold = compactField("180"); binarizationOptions.addView(row("Threshold", "0–255; one cutoff for the whole page", fixedThreshold), lp(match, wrap)) } } }
    private fun toggle(name: String, hint: String?, initial: Boolean): SquareToggle {
        val box = SquareToggle(initial); formBody.addView(row(name, hint, box).apply { setOnClickListener { box.flip() } }, lp(match, wrap)); return box
    }
    private fun row(name: String, hint: String?, control: View) = LinearLayout(this).apply {
        gravity = Gravity.CENTER_VERTICAL; setPadding(dp(18), dp(10), dp(18), dp(10)); addView(LinearLayout(context).apply { orientation = LinearLayout.VERTICAL; addView(text(name, 15f, INK)); hint?.let { addView(text(it, 11.5f, MUTED), lp(wrap, wrap).top(2)) } }, lp(0, wrap, 1f))
        addView(control, lp(if (control is EditText) dp(92) else dp(28), if (control is EditText) dp(38) else dp(28)))
    }
    private fun buildFooter() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(dp(18), dp(12), dp(18), dp(14))
        background = bg(SURFACE)
        addView(rule(), lp(match, dp(2)).margins(0, -14, 0, 12))
        addView(button("RUN", true).apply { setOnClickListener { run() } }, lp(match, dp(52)))
        addView(LinearLayout(context).apply { addView(button("ABOUT", false).apply { setOnClickListener { showAbout() } }, lp(0, dp(42), .5f)); addView(View(context), lp(0, dp(42), .5f)) }, lp(match, dp(42)).top(8))
    }
    private fun idleButton() { footer.addView(button("RUN", true).apply { setOnClickListener { run() } }, lp(match, dp(52))); footer.addView(LinearLayout(this).apply { addView(button("ABOUT", false).apply { setOnClickListener { showAbout() } }, lp(0, dp(42), .5f)); addView(View(context), lp(0, dp(42), .5f)) }, lp(match, dp(42)).top(8)) }

    private fun run() { if (inputFile == null) notice("Choose a source PDF first.") else start(inputFile!!, outputUri) }
    private fun start(input: File, outputFolder: Uri?) {
        val stem = documentTitle.text.toString().substringBeforeLast('.', documentTitle.text.toString()) + "-lege"
        val output = runCatching { createOutputDocument(outputFolder, "$stem.pdf", "application/pdf") }.getOrElse { notice("Could not create the PDF output."); return }
        val epubOutput = if (epub.checked) runCatching { createOutputDocument(outputFolder, "$stem.epub", "application/epub+zip") }.getOrElse { contentResolver.delete(output, null, null); notice("Could not create the EPUB output."); return } else null
        form.visibility = View.GONE; footer.removeAllViews(); footer.addView(button("STOP", false).apply { setOnClickListener { taskId?.let(LegeNative::nativeCancel) } }, lp(match, dp(52)))
        val running = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(dp(18), dp(22), dp(18), dp(18)); addView(text("RUNNING", 10.5f, MUTED, true, .22f)); addView(text(documentTitle.text.toString(), 19f, INK, true), lp(match, wrap).top(10)) }
        val percent = text("0%", 72f, INK, true); running.addView(percent, lp(match, wrap).top(28)); val progress = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply { max = 100; isIndeterminate = false }; running.addView(progress, lp(match, dp(8)).top(18)); val status = text("Starting…", 13f, INK, true); running.addView(status, lp(match, wrap).top(12)); root.addView(running, 1, lp(match, 0, 1f))
        worker.execute { val nativeOut = File(cacheDir, "output-${System.currentTimeMillis()}.pdf"); val nativeEpub = epubOutput?.let { File(cacheDir, "output-${System.currentTimeMillis()}.epub") }; runCatching { val id = LegeNative.nativeStartJob(input.absolutePath, nativeOut.absolutePath, params(nativeEpub)); check(id != 0L); taskId = id; while (true) { val update = LegeNative.nativePollProgress(id, 250) ?: continue; main.post { val m = update.metrics; val p = if (m != null && m.pagesTotal > 0) m.encoded * 100 / m.pagesTotal else 0; percent.text = "$p%"; progress.progress = p; status.text = listOf(update.headline, update.detail, update.hint).filter(String::isNotBlank).joinToString("\n") }; if (update.isTerminal) { if (update.kind == LegeProgress.KIND_ERROR) error(update.detail); contentResolver.openOutputStream(output, "w").use { sink -> requireNotNull(sink); nativeOut.inputStream().use { it.copyTo(sink) } }; if (epubOutput != null && nativeEpub != null) contentResolver.openOutputStream(epubOutput, "w").use { sink -> requireNotNull(sink); nativeEpub.inputStream().use { it.copyTo(sink) } }; break } }; main.post { finished("FINISHED", if (epubOutput == null) "Output saved to ${displayName(output)}" else "PDF and EPUB saved") } }.onFailure { error -> contentResolver.delete(output, null, null); epubOutput?.let { contentResolver.delete(it, null, null) }; main.post { finished("PROCESSING FAILED", error.message ?: "Unknown error") } } }
    }
    private fun finished(heading: String, detail: String) { taskId = null; val running = root.getChildAt(1) as LinearLayout; running.removeAllViews(); running.addView(text(heading, 10.5f, if (heading == "FINISHED") MUTED else "#673F36", true, .22f)); running.addView(text(documentTitle.text.toString(), 19f, INK, true), lp(match, wrap).top(10)); running.addView(text(detail, 14f, INK), lp(match, wrap).top(24)); footer.removeAllViews(); footer.addView(button("PROCESS ANOTHER", true).apply { setOnClickListener { reset() } }, lp(match, dp(52))) }
    private fun reset() { root.removeViewAt(1); form.visibility = View.VISIBLE; footer.removeAllViews(); idleButton(); documentTitle.text = "Select a PDF"; saveMeta.text = if (outputUri == null) "Downloads (default)" else "Last output folder" }
    private fun savedTargetHeight() = getSharedPreferences("lege", MODE_PRIVATE).getInt("target_height", 1200).toString()
    private fun savedOutputFolder() = getSharedPreferences("lege", MODE_PRIVATE).getString("output_folder", null)?.let(Uri::parse)?.takeIf(::canWriteFolder)
    private fun rememberOutputFolder(folder: Uri) = getSharedPreferences("lege", MODE_PRIVATE).edit().putString("output_folder", folder.toString()).apply()
    private fun params(epubOut: File?) = LegeParams().apply { val height = this@MainActivity.targetHeight.text.toString().toIntOrNull()?.takeIf { it > 0 } ?: 1200; this.targetHeight = height; getSharedPreferences("lege", MODE_PRIVATE).edit().putInt("target_height", height).apply(); enableLayoutDetection = layout.checked; enableOcr = ocrMode.value >= 0 || epub.checked; highQualityOutput = highQuality.checked; invertInput = inverted.checked; this.pageRange = this@MainActivity.pageRange.text.toString().ifBlank { null }; marginMode = when (this@MainActivity.marginMode.value) { 0 -> "center"; 1 -> "crop"; 2 -> "reflow"; else -> "none" }; slowOcr = ocrMode.value == 1 || epub.checked; jpegCompat = this@MainActivity.jpegCompat.checked; ditherImages = this@MainActivity.imageHandling.value == 1; binarizationMode = when (this@MainActivity.binarizationMode.value) { 0 -> "adaptive"; 1 -> "threshold"; 2 -> "sauvola"; else -> "default" }; if (this@MainActivity.binarizationMode.value == 0) sauvolaK = this@MainActivity.sauvolaK.text.toString().toFloatOrNull()?.coerceIn(0f, 1f) ?: 0.05f; if (this@MainActivity.binarizationMode.value == 1) fixedThreshold = this@MainActivity.fixedThreshold.text.toString().toIntOrNull()?.coerceIn(0, 255) ?: 180; epubSidecarPath = epubOut?.absolutePath }

    private fun chooseInput() = startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT).apply { addCategory(Intent.CATEGORY_OPENABLE); type = "application/pdf"; addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION or Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION) }, INPUT)
    private fun chooseOutput() = startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply { addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION or Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION) }, OUTPUT)
    private fun canWriteFolder(folder: Uri) = contentResolver.persistedUriPermissions.any { it.uri == folder && it.isWritePermission } || checkUriPermission(folder, android.os.Process.myPid(), android.os.Process.myUid(), Intent.FLAG_GRANT_WRITE_URI_PERMISSION) == PackageManager.PERMISSION_GRANTED
    private fun createOutputDocument(folder: Uri?, name: String, mimeType: String): Uri {
        if (folder != null && canWriteFolder(folder)) { val document = DocumentsContract.buildDocumentUriUsingTree(folder, DocumentsContract.getTreeDocumentId(folder)); return requireNotNull(DocumentsContract.createDocument(contentResolver, document, mimeType, name)) }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) { val values = ContentValues().apply { put(android.provider.MediaStore.MediaColumns.DISPLAY_NAME, name); put(android.provider.MediaStore.MediaColumns.MIME_TYPE, mimeType); put(android.provider.MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/Lege") }; return requireNotNull(contentResolver.insert(android.provider.MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)) }
        throw IllegalStateException("Android 9 and earlier require selecting an output folder")
    }
    private fun notice(message: String) = Toast.makeText(this, message, Toast.LENGTH_SHORT).show()
    private fun applyTheme(index: Int, persist: Boolean = true) { val theme = themes.getOrElse(index) { themes.first() }; INK = theme.ink; GROUND = theme.ground; SURFACE = theme.surface; FIELD = theme.field; MUTED = theme.muted; ACCENT = theme.accent; LIGHT = theme.light; if (persist) getSharedPreferences("lege", MODE_PRIVATE).edit().putInt("theme", index).apply() }
    private fun showAbout() {
        val version = packageManager.getPackageInfo(packageName, 0).versionName ?: "unknown"
        val content = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(dp(22), dp(18), dp(22), dp(10)); addView(text("LEGE $version", 20f, INK, true, .08f)); addView(text("Theme", 12f, MUTED, true, .14f), lp(match, wrap).top(18)) }
        var dialog: AlertDialog? = null
        themes.forEachIndexed { index, theme -> content.addView(button(theme.name, false).apply { setOnClickListener { dialog?.dismiss(); applyTheme(index); setContentView(buildScreen()) } }, lp(match, dp(42)).top(6)) }
        dialog = AlertDialog.Builder(this).setView(content).setNegativeButton("CLOSE", null).create(); dialog.show()
    }
    private fun copyInput(uri: Uri): File { val file = File(cacheDir, "input-${System.currentTimeMillis()}.pdf"); contentResolver.openInputStream(uri).use { source -> requireNotNull(source); file.outputStream().use { source.copyTo(it) } }; return file }
    private fun displayName(uri: Uri) = contentResolver.query(uri, null, null, null, null)?.use { cursor -> cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME).takeIf { it >= 0 }?.let { if (cursor.moveToFirst()) cursor.getString(it) else null } } ?: "Selected document"
    private fun action(left: String, right: String, click: () -> Unit) = LinearLayout(this).apply { gravity = Gravity.CENTER_VERTICAL; addView(text(left, 10.5f, MUTED, true, .16f), lp(0, wrap, 1f)); addView(text(right, 12.5f, ACCENT, true).apply { setOnClickListener { click() } }) }
    private fun button(value: String, filled: Boolean) = Button(this).apply { text = value; gravity = Gravity.START or Gravity.CENTER_VERTICAL; setPadding(dp(16), 0, dp(16), 0); textSize = 15f; letterSpacing = .08f; typeface = Typeface.DEFAULT_BOLD; setTextColor(c(if (filled) LIGHT else INK)); background = bg(if (filled) ACCENT else SURFACE, if (filled) 0 else 2) }
    private fun rule() = View(this).apply { setBackgroundColor(c(INK)) }; private fun text(value: String, size: Float, tint: String, bold: Boolean = false, spacing: Float = 0f) = TextView(this).apply { text = value; textSize = size; setTextColor(c(tint)); letterSpacing = spacing; if (bold) typeface = Typeface.DEFAULT_BOLD }
    private fun bg(fill: String, stroke: Int = 0) = GradientDrawable().apply { setColor(c(fill)); if (stroke > 0) setStroke(dp(stroke), c(INK)) }; private fun c(hex: String) = Color.parseColor(hex); private fun dp(n: Int) = (n * resources.displayMetrics.density).toInt(); private fun lp(w: Int, h: Int, weight: Float = 0f) = LinearLayout.LayoutParams(w, h, weight); private fun LinearLayout.LayoutParams.top(n: Int) = apply { topMargin = dp(n) }; private fun LinearLayout.LayoutParams.margins(l: Int, t: Int, r: Int, b: Int) = apply { setMargins(dp(l), dp(t), dp(r), dp(b)) }; private val match = ViewGroup.LayoutParams.MATCH_PARENT; private val wrap = ViewGroup.LayoutParams.WRAP_CONTENT
    private inner class SquareToggle(initial: Boolean) : TextView(this@MainActivity) { var checked = initial; private set; var onChanged: ((Boolean) -> Unit)? = null; init { gravity = Gravity.CENTER; textSize = 16f; render() }; fun flip() = setChecked(!checked); fun setChecked(value: Boolean) { checked = value; render(); onChanged?.invoke(value) }; private fun render() { text = if (checked) "✓" else ""; setTextColor(c(LIGHT)); background = bg(if (checked) ACCENT else FIELD, 2) } }
    private inner class SegmentedChoice(val items: List<String>, initial: Int, private val allowEmpty: Boolean = false) : LinearLayout(this@MainActivity) { var value = initial; private set; var onChanged: ((Int) -> Unit)? = null; init { orientation = HORIZONTAL; background = bg(GROUND, 2); draw() }; fun select(index: Int) { value = index; draw(); onChanged?.invoke(value) }; private fun draw() { removeAllViews(); items.forEachIndexed { index, item -> addView(text(item, 12f, if (index == value) LIGHT else INK, true).apply { gravity = Gravity.CENTER; setPadding(0, dp(10), 0, dp(10)); background = bg(if (index == value) ACCENT else GROUND); setOnClickListener { select(if (allowEmpty && value == index) -1 else index) } }, lp(0, wrap, 1f)) } } }
}
