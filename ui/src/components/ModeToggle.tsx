import { useGraphStore } from '../store/graphStore';

const modes = [
  { key: 'canvas' as const, label: 'Canvas', icon: '🎨' },
  { key: 'builder' as const, label: 'Builder', icon: '🔧' },
  { key: 'developer' as const, label: 'Developer', icon: '💻' },
] as const;

export function ModeToggle() {
  const { uiMode, setUiMode } = useGraphStore();

  return (
    <div style={{
      display: 'flex',
      gap: '4px',
      padding: '8px 16px',
      background: '#16213e',
      borderBottom: '1px solid #0f3460',
    }}>
      <span style={{ color: '#e0e0e0', marginRight: '12px', fontWeight: 'bold' }}>
        Nexus
      </span>
      {modes.map(({ key, label, icon }) => (
        <button
          key={key}
          onClick={() => setUiMode(key)}
          style={{
            padding: '4px 12px',
            border: 'none',
            borderRadius: '4px',
            cursor: 'pointer',
            background: uiMode === key ? '#0f3460' : 'transparent',
            color: uiMode === key ? '#e94560' : '#a0a0a0',
            fontWeight: uiMode === key ? 'bold' : 'normal',
            fontSize: '13px',
          }}
        >
          {icon} {label}
        </button>
      ))}
    </div>
  );
}
