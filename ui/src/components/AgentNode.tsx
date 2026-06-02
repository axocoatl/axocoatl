import { Handle, Position, type NodeProps } from '@xyflow/react';
import { useGraphStore } from '../store/graphStore';

interface AgentData {
  label: string;
  provider: string;
  model: string;
  [key: string]: unknown;
}

export function AgentNode({ id, data }: NodeProps) {
  const uiMode = useGraphStore((s) => s.uiMode);
  const tokenStatus = useGraphStore((s) => s.tokenStatus[id]);
  const agentData = data as AgentData;

  return (
    <div style={{
      padding: '12px 16px',
      borderRadius: '8px',
      border: '2px solid #0f3460',
      background: '#16213e',
      color: '#e0e0e0',
      minWidth: uiMode === 'canvas' ? '120px' : '200px',
      fontSize: '13px',
    }}>
      <Handle type="target" position={Position.Left} />

      {/* Canvas mode: minimal */}
      {uiMode === 'canvas' && (
        <div style={{ textAlign: 'center' }}>
          <div style={{ fontSize: '20px', marginBottom: '4px' }}>🤖</div>
          <div style={{ fontWeight: 'bold' }}>{agentData.label}</div>
        </div>
      )}

      {/* Builder mode: with details */}
      {uiMode === 'builder' && (
        <div>
          <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: '4px' }}>
            <span style={{ fontWeight: 'bold' }}>{agentData.label}</span>
            {tokenStatus && (
              <span style={{
                fontSize: '10px',
                background: tokenStatus.used > tokenStatus.budget * 0.8 ? '#e94560' : '#533483',
                padding: '2px 6px',
                borderRadius: '8px',
              }}>
                {tokenStatus.used}/{tokenStatus.budget}
              </span>
            )}
          </div>
          <div style={{ fontSize: '11px', color: '#a0a0a0' }}>
            {agentData.provider}/{agentData.model}
          </div>
        </div>
      )}

      {/* Developer mode: code view */}
      {uiMode === 'developer' && (
        <div>
          <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: '4px' }}>
            <code style={{ fontSize: '11px', color: '#e94560' }}>{id}</code>
            <span style={{ cursor: 'pointer', fontSize: '10px' }} title="Set breakpoint">⬤</span>
          </div>
          <pre style={{
            fontSize: '10px',
            background: '#0a0a1a',
            padding: '4px 8px',
            borderRadius: '4px',
            margin: 0,
            whiteSpace: 'pre-wrap',
          }}>
{`provider: ${agentData.provider}
model: ${agentData.model}`}
          </pre>
        </div>
      )}

      <Handle type="source" position={Position.Right} />
    </div>
  );
}
