package app.airhand.android.ui.session

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.outlined.LinkOff
import androidx.compose.material.icons.outlined.Monitor
import androidx.compose.material.icons.outlined.ScreenRotation
import androidx.compose.material.icons.outlined.Tune
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import app.airhand.android.AirhandApp
import app.airhand.android.R
import app.airhand.android.controller.RemoteSession
import app.airhand.android.core.Code
import app.airhand.android.core.Codec
import app.airhand.android.core.ExtendState
import app.airhand.android.core.OsKind
import app.airhand.android.core.Preset
import app.airhand.android.core.VirtualDisplaySpec
import app.airhand.android.data.ScaleMode
import app.airhand.android.ui.ConfirmDialog
import app.airhand.android.ui.phone.presetDesc
import app.airhand.android.ui.phone.presetName
import kotlinx.coroutines.delay
import kotlin.math.roundToInt

private data class MonitorPreset(val ratio: String, val spec: VirtualDisplaySpec)

private val MONITOR_PRESETS = listOf(
    MonitorPreset("16:10", VirtualDisplaySpec(1920, 1200)),
    MonitorPreset("16:10", VirtualDisplaySpec(2560, 1600)),
    MonitorPreset("16:9", VirtualDisplaySpec(1920, 1080)),
    MonitorPreset("16:9", VirtualDisplaySpec(2560, 1440)),
    MonitorPreset("4:3", VirtualDisplaySpec(1600, 1200)),
    MonitorPreset("4:3", VirtualDisplaySpec(2048, 1536)),
    MonitorPreset("3:2", VirtualDisplaySpec(1920, 1280)),
)

/**
 * 无线副屏专用界面：让平板 / 手机像一块纯粹的无线显示器一样工作。
 *
 * * 不进入远程控制界面、不绘制虚拟鼠标、不拦截触摸为远程鼠标/键盘操作；
 * * 对方电脑以 `draw_cursor = true` 把真实系统鼠标画进副屏画面：鼠标在主屏时不显示，移入副屏时自然出现；
 * * 单击屏幕呼出简洁的显示器 OSD 悬浮条（4 秒自动隐藏），提供分辨率/比例、清晰度/画质、编码格式、
 *   画面缩放、状态浮层、声音与断开连接控制。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MonitorSession(
    session: RemoteSession,
    view: RemoteView,
    landscape: Boolean,
    onRotate: () -> Unit,
    onExit: () -> Unit,
) {
    val ctx = LocalContext.current
    val app = AirhandApp.instance
    val video by session.videoSize.collectAsState()
    val rendered by session.frameRendered.collectAsState()
    val phase by session.phase.collectAsState()
    val stats by session.stats.collectAsState()
    val encoder by session.encoder.collectAsState()
    val decoder by session.decoderStats.collectAsState()
    val extendState by session.extendState.collectAsState()
    val extendDisplay by session.extendDisplay.collectAsState()
    val extendDetail by session.extendDetail.collectAsState()
    val extendSpec by session.extendSpec.collectAsState()
    val hostName by session.hostName.collectAsState()
    val settings by app.settings.flow.collectAsState()
    val os = session.os

    var osdVisible by remember { mutableStateOf(true) }
    var settingsOpen by remember { mutableStateOf(false) }
    var confirmExit by remember { mutableStateOf(false) }
    var scaleMode by remember { mutableStateOf(ScaleMode.FIT) }
    var showStats by remember { mutableStateOf(settings.showStats) }

    LaunchedEffect(video) { video?.let { view.setVideoSize(it.first, it.second) } }
    LaunchedEffect(scaleMode) { view.scaleMode = scaleMode }

    // 画面正常显示后，OSD 悬浮条 4 秒自动隐藏
    LaunchedEffect(osdVisible, settingsOpen, rendered) {
        if (osdVisible && !settingsOpen && rendered) {
            delay(4000)
            osdVisible = false
        }
    }

    BackHandler {
        when {
            settingsOpen -> settingsOpen = false
            else -> confirmExit = true
        }
    }

    val who = hostName.ifBlank { Code.format(session.code) }

    Box(
        Modifier
            .fillMaxSize()
            .background(Color.Black)
            .pointerInput(Unit) {
                detectTapGestures(onTap = { osdVisible = !osdVisible })
            },
    ) {
        AndroidView(factory = { view }, modifier = Modifier.fillMaxSize())

        // 顶部状态浮层（可选常显）
        if (showStats && rendered && !osdVisible) {
            val parts = buildList {
                encoder?.let { add(it.codec.label) }
                video?.let { add("${it.first}×${it.second}") }
                stats?.let { st ->
                    add("${st.fps.roundToInt()} fps")
                    st.rttMs?.let { add("${it.roundToInt()} ms") }
                    add(stringResource(if (st.direct) R.string.path_direct else R.string.path_relayed))
                }
                decoder?.decodeMs?.let { add("${it.roundToInt()} ms dec") }
            }
            if (parts.isNotEmpty()) {
                Text(
                    parts.joinToString(" · "),
                    color = Color.White.copy(alpha = 0.85f),
                    style = MaterialTheme.typography.labelSmall,
                    modifier = Modifier
                        .align(Alignment.TopStart)
                        .safeDrawingPadding()
                        .padding(12.dp)
                        .clip(RoundedCornerShape(8.dp))
                        .background(Color.Black.copy(alpha = 0.55f))
                        .padding(horizontal = 10.dp, vertical = 5.dp),
                )
            }
        }

        // 顶部简洁 OSD 悬浮条（单击画面呼出 / 自动隐藏）
        AnimatedVisibility(
            visible = osdVisible && phase == RemoteSession.Phase.Live && rendered,
            enter = fadeIn(),
            exit = fadeOut(),
            modifier = Modifier
                .align(Alignment.TopCenter)
                .safeDrawingPadding()
                .padding(top = 12.dp, start = 16.dp, end = 16.dp),
        ) {
            Row(
                Modifier
                    .clip(RoundedCornerShape(28.dp))
                    .background(Color(0xD916181D))
                    .padding(start = 16.dp, end = 6.dp, top = 4.dp, bottom = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(Icons.Outlined.Monitor, null, tint = Color.White, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(10.dp))
                Column(Modifier.widthIn(max = 260.dp)) {
                    Text(
                        who,
                        color = Color.White,
                        style = MaterialTheme.typography.labelLarge,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    val meta = buildList {
                        add("${extendSpec.width}×${extendSpec.height}")
                        encoder?.let { add(it.codec.label) }
                        stats?.let { st ->
                            add("${st.fps.roundToInt()} fps")
                            st.rttMs?.let { add("${it.roundToInt()} ms") }
                        }
                    }.joinToString(" · ")
                    Text(
                        meta,
                        color = Color.White.copy(alpha = 0.72f),
                        style = MaterialTheme.typography.labelSmall,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                Spacer(Modifier.width(8.dp))
                IconButton(onClick = { settingsOpen = true }) {
                    Icon(Icons.Outlined.Tune, stringResource(R.string.monitor_settings), tint = Color.White)
                }
                IconButton(onClick = onRotate) {
                    Icon(
                        Icons.Outlined.ScreenRotation,
                        stringResource(if (landscape) R.string.menu_to_portrait else R.string.menu_to_landscape),
                        tint = Color.White,
                    )
                }
                IconButton(onClick = { confirmExit = true }) {
                    Icon(Icons.Outlined.LinkOff, stringResource(R.string.disconnect), tint = Color(0xFFFF8A80))
                }
            }
        }

        // 副屏准备中 / 失败 / 连接状态盖层
        val statusCovered = SessionStatus(session, big = false, onExit = onExit)
        if (!statusCovered && phase == RemoteSession.Phase.Live) {
            val st = extendState
            val errorMsg: String? = when (st) {
                ExtendState.UNAVAILABLE -> stringResource(
                    when (os) {
                        OsKind.MAC -> R.string.extend_unavailable_mac
                        OsKind.LINUX -> R.string.extend_unavailable_linux
                        else -> R.string.extend_unavailable_windows
                    },
                )
                ExtendState.NOT_ALLOWED -> stringResource(R.string.extend_not_allowed)
                ExtendState.FAILED -> stringResource(R.string.extend_failed, extendDetail ?: "—")
                ExtendState.OFF -> stringResource(R.string.monitor_stopped)
                else -> null
            }
            if (errorMsg != null) {
                Box(
                    Modifier
                        .fillMaxSize()
                        .background(Color.Black.copy(alpha = 0.85f)),
                    contentAlignment = Alignment.Center,
                ) {
                    Column(
                        Modifier
                            .widthIn(max = 420.dp)
                            .padding(32.dp),
                        horizontalAlignment = Alignment.CenterHorizontally,
                    ) {
                        Icon(Icons.Outlined.ErrorOutline, null, tint = Color(0xFFFF8A80), modifier = Modifier.size(48.dp))
                        Spacer(Modifier.height(16.dp))
                        Text(
                            stringResource(R.string.monitor_unavailable_title),
                            color = Color.White,
                            style = MaterialTheme.typography.titleMedium,
                            textAlign = TextAlign.Center,
                        )
                        Spacer(Modifier.height(8.dp))
                        Text(
                            errorMsg,
                            color = Color.White.copy(alpha = 0.78f),
                            style = MaterialTheme.typography.bodyMedium,
                            textAlign = TextAlign.Center,
                        )
                        Spacer(Modifier.height(24.dp))
                        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                            if (st == ExtendState.FAILED || st == ExtendState.OFF) {
                                Button(onClick = { session.setExtend(extendSpec) }) {
                                    Text(stringResource(R.string.retry))
                                }
                            }
                            OutlinedButton(onClick = onExit) {
                                Text(stringResource(R.string.disconnect), color = Color.White)
                            }
                        }
                    }
                }
            } else if (!rendered || st == null || st == ExtendState.STARTING) {
                Box(
                    Modifier
                        .fillMaxSize()
                        .background(Color.Black.copy(alpha = 0.78f)),
                    contentAlignment = Alignment.Center,
                ) {
                    Column(
                        Modifier
                            .widthIn(max = 420.dp)
                            .padding(32.dp),
                        horizontalAlignment = Alignment.CenterHorizontally,
                    ) {
                        CircularProgressIndicator(Modifier.size(42.dp), color = Color.White)
                        Spacer(Modifier.height(20.dp))
                        Text(
                            stringResource(R.string.monitor_setting_up, extendSpec.width, extendSpec.height),
                            color = Color.White,
                            style = MaterialTheme.typography.titleMedium,
                            textAlign = TextAlign.Center,
                        )
                        Spacer(Modifier.height(8.dp))
                        Text(
                            stringResource(R.string.monitor_setting_up_hint),
                            color = Color.White.copy(alpha = 0.72f),
                            style = MaterialTheme.typography.bodySmall,
                            textAlign = TextAlign.Center,
                        )
                        Spacer(Modifier.height(24.dp))
                        OutlinedButton(onClick = onExit) {
                            Text(stringResource(R.string.cancel), color = Color.White)
                        }
                    }
                }
            }
        }
    }

    if (settingsOpen) {
        MonitorSettingsSheet(
            session = session,
            os = os,
            extendDisplay = extendDisplay,
            scaleMode = scaleMode,
            showStats = showStats,
            onScaleMode = { scaleMode = it },
            onShowStats = { showStats = it },
            onDisconnect = {
                settingsOpen = false
                confirmExit = true
            },
            onDismiss = { settingsOpen = false },
        )
    }

    if (confirmExit) {
        ConfirmDialog(
            title = stringResource(R.string.disconnect_title),
            text = stringResource(R.string.monitor_disconnect_text, who),
            confirm = stringResource(R.string.disconnect),
            onDismiss = { confirmExit = false },
            onConfirm = {
                confirmExit = false
                onExit()
            },
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
private fun MonitorSettingsSheet(
    session: RemoteSession,
    os: OsKind,
    extendDisplay: String?,
    scaleMode: ScaleMode,
    showStats: Boolean,
    onScaleMode: (ScaleMode) -> Unit,
    onShowStats: (Boolean) -> Unit,
    onDisconnect: () -> Unit,
    onDismiss: () -> Unit,
) {
    val ctx = LocalContext.current
    val app = AirhandApp.instance
    val spec by session.extendSpec.collectAsState()
    val quality by session.quality.collectAsState()
    val encoder by session.encoder.collectAsState()
    val audioOn by session.audioOn.collectAsState()
    val settings by app.settings.flow.collectAsState()
    val dm = ctx.resources.displayMetrics
    val nativeSpec = remember(dm.widthPixels, dm.heightPixels, spec.hidpi) {
        VirtualDisplaySpec.matchScreen(dm.widthPixels, dm.heightPixels, hidpi = spec.hidpi)
    }

    fun pickSpec(next: VirtualDisplaySpec) {
        session.setExtend(next.copy(hidpi = if (os == OsKind.MAC) spec.hidpi else false))
    }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(bottom = 28.dp),
        ) {
            Row(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 24.dp, vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(Icons.Outlined.Monitor, null, tint = MaterialTheme.colorScheme.primary)
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text(stringResource(R.string.monitor_settings), style = MaterialTheme.typography.titleMedium)
                    Text(
                        stringResource(R.string.extend_on, extendDisplay ?: "—", spec.width, spec.height),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            // 1. 分辨率与纵横比
            HorizontalDivider(Modifier.padding(vertical = 10.dp, horizontal = 16.dp))
            Text(
                stringResource(R.string.extend_resolution),
                style = MaterialTheme.typography.titleSmall,
                color = MaterialTheme.colorScheme.primary,
                modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
            )
            FlowRow(
                Modifier.padding(horizontal = 20.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                FilterChip(
                    selected = spec.width == nativeSpec.width && spec.height == nativeSpec.height,
                    onClick = { pickSpec(nativeSpec) },
                    label = { Text(stringResource(R.string.extend_match_screen, nativeSpec.width, nativeSpec.height)) },
                )
                MONITOR_PRESETS.forEach { preset ->
                    if (preset.spec.width == nativeSpec.width && preset.spec.height == nativeSpec.height) return@forEach
                    val same = spec.width == preset.spec.width && spec.height == preset.spec.height
                    FilterChip(
                        selected = same,
                        onClick = { pickSpec(preset.spec) },
                        label = { Text("${preset.ratio} · ${preset.spec.width}×${preset.spec.height}") },
                    )
                }
                FilterChip(
                    selected = spec.height > spec.width,
                    onClick = { pickSpec(spec.swapped()) },
                    label = { Text(stringResource(R.string.extend_swap_orientation)) },
                )
                if (os == OsKind.MAC) {
                    FilterChip(
                        selected = spec.hidpi,
                        onClick = { session.setExtend(spec.copy(hidpi = !spec.hidpi)) },
                        label = { Text(stringResource(R.string.extend_hidpi)) },
                    )
                }
            }

            // 2. 清晰度 / 画质
            HorizontalDivider(Modifier.padding(vertical = 10.dp, horizontal = 16.dp))
            Text(
                stringResource(R.string.menu_quality),
                style = MaterialTheme.typography.titleSmall,
                color = MaterialTheme.colorScheme.primary,
                modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
            )
            val currentPreset = Preset.match(quality)
            Preset.entries.forEach { p ->
                Row(
                    Modifier
                        .fillMaxWidth()
                        .clickable { session.setQuality(p.quality(quality.codec).copy(drawCursor = true)) }
                        .padding(horizontal = 16.dp, vertical = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(selected = p == currentPreset, onClick = null)
                    Spacer(Modifier.width(12.dp))
                    Column {
                        Text(presetName(p), style = MaterialTheme.typography.bodyLarge)
                        Text(presetDesc(p), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }

            // 3. 编码格式
            val usableCodecs = session.usableCodecs()
            if (usableCodecs.size > 1) {
                HorizontalDivider(Modifier.padding(vertical = 10.dp, horizontal = 16.dp))
                Text(
                    stringResource(R.string.codec),
                    style = MaterialTheme.typography.titleSmall,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
                )
                val chosenCodec = Codec.fromWire(settings.codec)
                FlowRow(
                    Modifier.padding(horizontal = 20.dp),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    FilterChip(
                        selected = chosenCodec == null,
                        onClick = { session.setCodec(null) },
                        label = { Text(stringResource(R.string.codec_auto_value, usableCodecs.first().label)) },
                    )
                    usableCodecs.forEach { c ->
                        FilterChip(
                            selected = chosenCodec == c,
                            onClick = { session.setCodec(c) },
                            label = { Text(c.label) },
                        )
                    }
                }
            }
            encoder?.let { enc ->
                Text(
                    stringResource(
                        R.string.encoder_info,
                        enc.codec.label,
                        enc.width,
                        enc.height,
                        enc.name,
                        stringResource(if (enc.hardware) R.string.hardware else R.string.software),
                    ),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(horizontal = 24.dp, vertical = 6.dp),
                )
            }

            // 4. 缩放比例、状态浮层、声音
            HorizontalDivider(Modifier.padding(vertical = 10.dp, horizontal = 16.dp))
            Text(
                stringResource(R.string.monitor_display_options),
                style = MaterialTheme.typography.titleSmall,
                color = MaterialTheme.colorScheme.primary,
                modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
            )
            FlowRow(
                Modifier.padding(horizontal = 20.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                listOf(
                    ScaleMode.FIT to R.string.tv_scale_fit,
                    ScaleMode.FILL to R.string.tv_scale_fill,
                    ScaleMode.STRETCH to R.string.tv_scale_stretch,
                ).forEach { (mode, labelRes) ->
                    FilterChip(
                        selected = scaleMode == mode,
                        onClick = { onScaleMode(mode) },
                        label = { Text(stringResource(labelRes)) },
                    )
                }
            }
            ListItem(
                headlineContent = { Text(stringResource(R.string.show_stats)) },
                trailingContent = { Switch(checked = showStats, onCheckedChange = onShowStats) },
                colors = ListItemDefaults.colors(containerColor = Color.Transparent),
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable { onShowStats(!showStats) }
                    .padding(horizontal = 8.dp),
            )
            ListItem(
                headlineContent = { Text(stringResource(R.string.play_audio)) },
                trailingContent = { Switch(checked = audioOn, onCheckedChange = session::setAudio) },
                colors = ListItemDefaults.colors(containerColor = Color.Transparent),
                modifier = Modifier
                    .fillMaxWidth()
                    .clickable { session.setAudio(!audioOn) }
                    .padding(horizontal = 8.dp),
            )

            HorizontalDivider(Modifier.padding(vertical = 10.dp, horizontal = 16.dp))
            Box(Modifier.fillMaxWidth().padding(horizontal = 24.dp)) {
                OutlinedButton(
                    onClick = onDisconnect,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Icon(Icons.Outlined.LinkOff, null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(8.dp))
                    Text(stringResource(R.string.disconnect))
                }
            }
        }
    }
}
